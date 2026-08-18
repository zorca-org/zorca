mod thread_switcher;
pub mod workspace_manager;

use agent_settings::AgentSettings;
use agent_workspaces::row_display::{format_history_entry_timestamp, fuzzy_match_positions};
use agent_workspaces::terminal_thread_metadata_store::{
    TerminalThreadMetadata, TerminalThreadMetadataStore,
};
use agent_workspaces::thread_worktree_archive;
use agent_workspaces::worktree_info_from_thread_paths;
use agent_workspaces::{ArchiveSelectedThread, NewTerminalThread, TerminalId};
use chrono::{DateTime, Utc};
use editor::Editor;
use feature_flags::{AgentThreadWorktreeLabelFlag, FeatureFlag};
use gpui::{
    AnyElement, App, Context, Decorations, Entity, EntityId, FocusHandle, Focusable, KeyContext,
    ListState, Pixels, Render, SharedString, Task, TaskExt, WeakEntity, Window, prelude::*, px,
};
use itertools::Itertools;
use menu::{
    Cancel, Confirm, SelectChild, SelectFirst, SelectLast, SelectNext, SelectParent, SelectPrevious,
};
use project::WorktreePaths;
use project::{Event as ProjectEvent, WorktreeId};
use recent_projects::sidebar_recent_projects::SidebarRecentProjects;
use remote::RemoteConnectionOptions;

use ade_workspaces::DaemonUpgradeOutcome;
use serde::{Deserialize, Serialize};
use settings::Settings as _;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use theme::{ActiveTheme, CLIENT_SIDE_DECORATION_ROUNDING};
use ui::{
    ContextMenu, PopoverMenu, PopoverMenuHandle, ThreadItemWorktreeInfo, TintColor, Tooltip,
    prelude::*, right_click_menu,
};
use util::ResultExt as _;
use util::path_list::PathList;
use workspace::notifications::NotificationId;
use workspace::{
    MultiWorkspace, MultiWorkspaceEvent, NextProject, OpenMode, PreviousProject, ProjectGroupKey,
    SaveIntent, Sidebar as WorkspaceSidebar, SidebarSide, Toast, Workspace,
};

use zed_actions::OpenRecent;
use zed_actions::editor::{MoveDown, MoveUp};
#[cfg(test)]
use zed_actions::{CreateWorktree, NewWorktreeBranchTarget};

use zed_actions::agents_sidebar::{FocusSidebarFilter, ToggleThreadSwitcher};

use crate::thread_switcher::{
    ThreadSwitcher, ThreadSwitcherEntry, ThreadSwitcherEvent, ThreadSwitcherSelection,
    ThreadSwitcherTerminalEntry,
};

#[cfg(test)]
mod sidebar_tests;

gpui::actions!(
    agents_sidebar,
    [
        /// Creates a new thread in the currently selected or active project group.
        NewThreadInGroup,
        /// Toggles between the thread list and the thread history.
        ToggleThreadHistory,
    ]
);

gpui::actions!(
    dev,
    [
        /// Dumps multi-workspace state (projects, worktrees, active threads) into a new buffer.
        DumpWorkspaceInfo,
    ]
);

const DEFAULT_WIDTH: Pixels = px(300.0);
const MIN_WIDTH: Pixels = px(200.0);
const MAX_WIDTH: Pixels = px(800.0);

/// Names the toast the "upgrade host daemon" button raises, so a second click
/// replaces the first one's answer instead of stacking a second notification.
struct UpgradeHostDaemon;

struct RenameWorktree;

fn renamed_worktree_path(old_path: &Path, name: &str) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return None;
    }
    Some(old_path.parent()?.join(name))
}

fn closed_project_groups(
    open_keys: &[ProjectGroupKey],
    project_group_keys: Vec<ProjectGroupKey>,
) -> Vec<ProjectGroupKey> {
    project_group_keys
        .into_iter()
        .filter(|key| !key.path_list().paths().is_empty())
        .filter(|key| !open_keys.iter().any(|open_key| open_key.matches(key)))
        .collect()
}

fn workspace_for_scoped_root(
    multi_workspace: &MultiWorkspace,
    root: &Path,
    group_key: Option<&ProjectGroupKey>,
    cx: &App,
) -> Option<Entity<Workspace>> {
    let host_key =
        group_key.map(|group_key| workspace_manager::host_cache_key(group_key.host().as_ref()));
    multi_workspace
        .workspaces()
        .find(|workspace| {
            let workspace_host_key = workspace_manager::host_cache_key(
                workspace
                    .read(cx)
                    .project()
                    .read(cx)
                    .remote_connection_options(cx)
                    .as_ref(),
            );
            host_key
                .as_ref()
                .is_none_or(|host_key| &workspace_host_key == host_key)
                && workspace_path_list(workspace, cx)
                    .paths()
                    .iter()
                    .any(|path| path == root)
        })
        .cloned()
}

fn update_cached_worktree_path(
    available_worktrees: &mut workspace_manager::AvailableWorktrees,
    repository_key: &Path,
    host_key: Option<&str>,
    old_path: &Path,
    new_path: &Path,
) {
    let key = (
        repository_key.to_path_buf(),
        host_key.map(ToOwned::to_owned),
    );
    let Some(worktrees) = available_worktrees.get_mut(&key) else {
        return;
    };
    for worktree in worktrees
        .iter_mut()
        .filter(|worktree| worktree.path == old_path)
    {
        worktree.path = new_path.to_path_buf();
    }
}

/// The host of a row that draws the daemon upgrade arrow, given a `is_stale`
/// answer for a host. Only a project row has a host of its own to upgrade: a
/// group row spans hosts, and a worktree row's daemon is its project's.
///
/// Separate from `Sidebar::stale_daemon_host` so the rule can be tested without
/// an `App`; `is_stale` is the lifecycle service in production.
fn stale_daemon_host_for_row(
    kind: workspace_manager::RowKind,
    ade_host: Option<&str>,
    is_stale: impl FnOnce(&str) -> bool,
) -> Option<String> {
    if !matches!(kind, workspace_manager::RowKind::Project(_)) {
        return None;
    }
    let host = ade_host?;
    is_stale(host).then(|| host.to_owned())
}

/// What a workspace-manager row's menus act on.
#[derive(Clone)]
struct WorkspaceRowContext {
    kind: workspace_manager::RowKind,
    host_key: Option<String>,
    /// The workspace a project or worktree row acts on. `None` for a group row
    /// holding no projects.
    workspace_key: Option<ProjectGroupKey>,
    /// Every persisted identity represented by a visually merged project row.
    removal_keys: Vec<ProjectGroupKey>,
    /// Which user-created group this row is, by index into `workspace_groups`.
    group_index: Option<usize>,
    /// Which project this row is, by its stable key.
    project_key: Option<PathBuf>,
    /// The project containing this worktree row.
    worktree_project_key: Option<PathBuf>,
    /// The worktree this row is, when Git would actually let us remove it.
    /// `None` for the main checkout, which Git refuses to remove.
    removable_worktree: Option<PathBuf>,
    worktree_workspace: Option<WeakEntity<Workspace>>,
    /// Whether this row can create a worktree. False for a project under no
    /// version control, where the action silently did nothing.
    can_create_worktree: bool,
    /// The root of the worktree this row is, pinnable whether or not Git would
    /// let us remove it.
    worktree_root: Option<PathBuf>,
    worktree_name: Option<SharedString>,
    has_hidden_worktrees: bool,
    shows_hidden_worktrees: bool,
    worktree_is_hidden: bool,
    can_hide_worktree: bool,
    /// The ssh destination this row's project lives on, as ADE names a host.
    /// `Some` only for a remote row, which is the only kind with a session
    /// daemon of its own to upgrade — the local one ships inside the app.
    ade_host: Option<String>,
}

#[derive(Clone)]
enum WorkspaceCollapseKey {
    Global(SharedString),
    Project {
        key: workspace_manager::ScopedPath,
        legacy_key: SharedString,
    },
}

#[derive(Clone, Copy)]
enum NewEntryTarget {
    LastCreatedKind,
    Terminal,
    /// Selecting a worktree has to leave the user somewhere they can work, but
    /// re-selecting one that is already open must not stack up terminals.
    TerminalIfCentreEmpty,
}

#[derive(Default, Serialize, Deserialize)]
struct SerializedSidebar {
    #[serde(default)]
    width: Option<f32>,
    #[serde(default)]
    workspace_groups: Vec<SerializedWorkspaceGroup>,
    #[serde(default)]
    collapsed_workspace_nodes: Vec<String>,
    #[serde(default)]
    pinned_worktrees: Vec<workspace_manager::ScopedPath>,
    #[serde(default)]
    unread_worktrees: Vec<workspace_manager::ScopedPath>,
    #[serde(default)]
    hidden_worktrees: Vec<workspace_manager::ScopedPath>,
    #[serde(default)]
    collapsed_projects: Vec<workspace_manager::ScopedPath>,
}

/// A user-created group of projects. Membership is stored by the project's
/// stable key rather than its name, so renaming a directory does not empty the
/// group.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SerializedWorkspaceGroup {
    name: String,
    #[serde(default)]
    projects: Vec<workspace_manager::ScopedPath>,
}

enum ArchiveWorktreeOutcome {
    Success,
    Cancelled,
}

#[derive(Clone, Debug)]
enum ActiveEntry {
    Terminal {
        terminal_id: TerminalId,
        workspace: Entity<Workspace>,
    },
}

impl ActiveEntry {
    fn is_active_terminal(&self, terminal_id: TerminalId) -> bool {
        matches!(self, ActiveEntry::Terminal { terminal_id: active_terminal_id, .. } if *active_terminal_id == terminal_id)
    }
}

#[derive(Clone)]
enum ThreadEntryWorkspace {
    Open(Entity<Workspace>),
    Closed {
        /// The paths this entry uses (may point to linked worktrees).
        folder_paths: PathList,
        /// The project group this entry belongs to.
        project_group_key: ProjectGroupKey,
    },
}

impl ThreadEntryWorkspace {}

#[derive(Clone)]
struct TerminalEntry {
    metadata: TerminalThreadMetadata,
    workspace: ThreadEntryWorkspace,
    worktrees: Vec<ThreadItemWorktreeInfo>,
    has_notification: bool,
    highlight_positions: Vec<usize>,
}

#[derive(Clone)]
enum ListEntry {
    ProjectHeader {
        key: ProjectGroupKey,
        label: SharedString,
        has_entries: bool,
    },
    Terminal(TerminalEntry),
}

#[derive(Clone)]
enum ActivatableEntry {
    Terminal {
        metadata: TerminalThreadMetadata,
        workspace: ThreadEntryWorkspace,
    },
}

impl ActivatableEntry {
    fn from_list_entry(entry: &ListEntry) -> Option<Self> {
        match entry {
            ListEntry::Terminal(terminal) => Some(Self::Terminal {
                metadata: terminal.metadata.clone(),
                workspace: terminal.workspace.clone(),
            }),
            ListEntry::ProjectHeader { .. } => None,
        }
    }

    fn project_location(&self, cx: &App) -> (PathList, ProjectGroupKey) {
        match self {
            Self::Terminal {
                workspace: ThreadEntryWorkspace::Open(workspace),
                ..
            } => (
                PathList::new(&workspace.read(cx).root_paths(cx)),
                workspace.read(cx).project_group_key(cx),
            ),
            Self::Terminal {
                workspace:
                    ThreadEntryWorkspace::Closed {
                        folder_paths,
                        project_group_key,
                    },
                ..
            } => (folder_paths.clone(), project_group_key.clone()),
        }
    }
}

#[cfg(test)]
impl ListEntry {}

impl From<TerminalEntry> for ListEntry {
    fn from(terminal: TerminalEntry) -> Self {
        ListEntry::Terminal(terminal)
    }
}

#[derive(Default)]
struct SidebarContents {
    entries: Vec<ListEntry>,
    notified_terminals: HashSet<TerminalId>,
    project_header_indices: Vec<usize>,
}

/// Identity-and-layout key for a [`ListEntry`] used to preserve measured list items
/// across rebuilds. Equal shapes must render to the same height; add any new
/// height-affecting state here.
#[derive(Debug, PartialEq, Eq)]
enum EntryShape {
    ProjectHeader {
        key: ProjectGroupKey,
        // Toggles the "No threads yet" empty-state row when not collapsed.
        has_entries: bool,
        // Determines whether the "No threads yet" row is rendered (only shown when
        // `!is_collapsed && !has_threads`).
        is_collapsed: bool,
    },
    Terminal(TerminalId),
}

impl SidebarContents {
    fn is_terminal_notified(&self, terminal_id: TerminalId) -> bool {
        self.notified_terminals.contains(&terminal_id)
    }
}

// TODO: The mapping from workspace root paths to git repositories needs a
// unified approach across the codebase: this function, `AgentPanel::classify_worktrees`,
// thread persistence (which PathList is saved to the database), and thread
// querying (which PathList is used to read threads back). All of these need
// to agree on how projects are resolved for a given workspace, especially in
// multi-root and nested-project configurations.
fn root_repository_snapshots(
    workspace: &Entity<Workspace>,
    cx: &App,
) -> impl Iterator<Item = project::git_store::RepositorySnapshot> {
    let path_list = workspace_path_list(workspace, cx);
    let project = workspace.read(cx).project().read(cx);
    project
        .repositories(cx)
        .values()
        .filter_map(move |project| {
            let snapshot = project.read(cx).snapshot();
            let is_root = path_list
                .paths()
                .iter()
                .any(|p| p.as_path() == snapshot.work_directory_abs_path.as_ref());
            is_root.then_some(snapshot)
        })
}

fn workspace_path_list(workspace: &Entity<Workspace>, cx: &App) -> PathList {
    PathList::new(&workspace.read(cx).root_paths(cx))
}

fn linked_worktree_path_lists_for_workspaces(
    workspaces: &[Entity<Workspace>],
    cx: &App,
) -> Vec<PathList> {
    let mut linked_worktree_paths = Vec::new();
    for workspace in workspaces {
        if workspace.read(cx).visible_worktrees(cx).count() != 1 {
            continue;
        }
        for snapshot in root_repository_snapshots(workspace, cx) {
            linked_worktree_paths.extend(
                snapshot.linked_worktrees().iter().map(|linked_worktree| {
                    PathList::new(std::slice::from_ref(&linked_worktree.path))
                }),
            );
        }
    }

    linked_worktree_paths.sort_by(|a, b| a.paths()[0].cmp(&b.paths()[0]));
    linked_worktree_paths
}

fn workspace_has_terminal_metadata_except(
    workspace: &Entity<Workspace>,
    except_terminal_id: Option<TerminalId>,
    cx: &App,
) -> bool {
    let Some(store) = TerminalThreadMetadataStore::try_global(cx) else {
        return false;
    };
    let path_list = workspace_path_list(workspace, cx);
    let remote_connection = workspace
        .read(cx)
        .project()
        .read(cx)
        .remote_connection_options(cx);
    store
        .read(cx)
        .entries_for_path(&path_list, remote_connection.as_ref())
        .any(|terminal| except_terminal_id != Some(terminal.terminal_id))
}

/// Shows a [`RemoteConnectionModal`] on the given workspace and establishes
/// an SSH connection. Suitable for passing to
/// [`MultiWorkspace::find_or_create_workspace`] as the `connect_remote`
/// argument.
fn connect_remote(
    modal_workspace: Entity<Workspace>,
    connection_options: RemoteConnectionOptions,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) -> gpui::Task<anyhow::Result<Option<Entity<remote::RemoteClient>>>> {
    remote_connection::connect_with_modal(&modal_workspace, connection_options, window, cx)
}

/// The sidebar re-derives its entire entry list from scratch on every
/// change via `update_entries` → `rebuild_contents`. Avoid adding
/// incremental or inter-event coordination state — if something can
/// be computed from the current world state, compute it in the rebuild.
pub struct Sidebar {
    multi_workspace: WeakEntity<MultiWorkspace>,
    width: Pixels,
    focus_handle: FocusHandle,
    filter_editor: Entity<Editor>,
    thread_rename_editor: Entity<Editor>,
    list_state: ListState,
    contents: SidebarContents,
    /// Workspace-manager nodes the user collapsed, keyed by "group" or
    /// "group/project". Keyed by name rather than id because the tree is rebuilt
    /// from the open workspaces on every render, which reassigns ids.
    collapsed_workspace_nodes: HashSet<SharedString>,
    collapsed_projects: HashSet<workspace_manager::ScopedPath>,
    pending_worktree_open: Option<PathBuf>,
    pending_worktree_deletions: HashSet<workspace_manager::ScopedPath>,
    pending_worktree_renames: HashSet<(PathBuf, Option<String>, PathBuf)>,
    /// Set when the user asks to add a project, so the workspace that add
    /// produces opens a terminal while restored ones do not.
    open_terminal_for_next_workspace: bool,
    /// User-created groups, in display order.
    workspace_groups: Vec<SerializedWorkspaceGroup>,
    /// Roots of the worktrees the user pinned, in pin order.
    pinned_worktrees: Vec<workspace_manager::ScopedPath>,
    /// Roots of the worktrees the user has not caught up with.
    unread_worktrees: Vec<workspace_manager::ScopedPath>,
    /// Git worktrees hidden by the user, persisted by checkout path.
    hidden_worktrees: Vec<workspace_manager::ScopedPath>,
    /// Projects temporarily revealing their hidden worktrees.
    projects_showing_hidden_worktrees: HashSet<workspace_manager::ScopedPath>,
    /// Git's complete worktree list for each open repository.
    available_worktrees: workspace_manager::AvailableWorktrees,
    available_worktrees_refresh_id: usize,
    /// The group whose name is being edited inline, by index.
    renaming_workspace_group: Option<usize>,
    group_rename_editor: Entity<Editor>,
    renaming_worktree: Option<(PathBuf, Option<String>, Option<PathBuf>)>,
    renaming_worktree_name: Option<SharedString>,
    worktree_rename_editor: Entity<Editor>,
    /// The index of the list item that currently has the keyboard focus
    ///
    /// Note: This is NOT the same as the active item.
    selection: Option<usize>,
    /// Tracks which sidebar entry is currently active (highlighted).
    active_entry: Option<ActiveEntry>,
    renaming_thread_id: Option<TerminalId>,
    /// Threads in the database-backed regeneration path need their own loading
    /// state because they do not have a live `agent::Thread` to report it.
    /// start_renaming_thread must seed current title into the title editor
    /// so this prevents that BufferEdited event from being interpreted as user input.
    suppress_next_rename_edit: bool,

    /// Updated only in response to explicit user actions (clicking a
    /// thread, confirming in the thread switcher, etc.) — never from
    /// background data changes. Used to sort the thread switcher popup.
    terminal_last_accessed: HashMap<TerminalId, DateTime<Utc>>,
    thread_switcher: Option<Entity<ThreadSwitcher>>,
    _thread_switcher_subscriptions: Vec<gpui::Subscription>,
    pending_thread_activation: Option<TerminalId>,
    /// Persists live thread statuses across rebuilds so that Running→Completed
    /// transitions can be detected even when the group is collapsed (and
    /// thread entries are not present in the list).
    /// Remembers whether each draft last rendered as empty or with content so
    /// that when a draft that was empty gains content again, we refresh
    /// its interaction time.
    recent_projects_popover_handle: PopoverMenuHandle<SidebarRecentProjects>,
    /// The header's "Add Project" menu. Separate from the footer's handle so
    /// opening one does not toggle the other, which shares its menu type.
    add_project_popover_handle: PopoverMenuHandle<SidebarRecentProjects>,
    _subscriptions: Vec<gpui::Subscription>,
    _draft_editor_observations: Vec<gpui::Subscription>,
    update_task: Option<Task<()>>,
    /// Redraws the rows when a host's daemon starts or stops being behind this
    /// client's. Held rather than detached, so closing the window drops the
    /// receiver and the lifecycle service stops keeping a sender for it.
    _daemon_freshness_watch: Task<()>,
}

impl Sidebar {
    fn open_worktree_picker(
        &self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_workspace(workspace, window, cx);
        workspace.update(cx, |workspace, cx| {
            git_ui::worktree_picker::toggle(workspace, window, cx)
        });
    }

    #[cfg(test)]
    fn create_worktree(
        &mut self,
        workspace: &Entity<Workspace>,
        worktree_name: Option<String>,
        branch_target: NewWorktreeBranchTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task = workspace.update(cx, |workspace, cx| {
            let focused_dock = workspace.focused_dock_position(window, cx);
            git_ui::worktree_service::create_and_activate_worktree_workspace(
                workspace,
                &CreateWorktree {
                    worktree_name,
                    branch_target,
                },
                window,
                focused_dock,
                cx,
            )
        });

        cx.spawn_in(window, async move |this, cx| {
            let created = task.await?;
            this.update_in(cx, |this, window, cx| {
                this.selection = None;
                let is_remote = created
                    .workspace
                    .read(cx)
                    .project()
                    .read(cx)
                    .remote_connection_options(cx)
                    .is_some();
                if !is_remote
                    && created
                        .workspace
                        .read(cx)
                        .active_pane()
                        .read(cx)
                        .items_len()
                        == 0
                {
                    this.create_new_terminal(&created.workspace, window, cx);
                }
                this.update_entries(cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn new(
        multi_workspace: Entity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        cx.on_focus_in(&focus_handle, window, Self::focus_in)
            .detach();

        AgentThreadWorktreeLabelFlag::watch(cx);

        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            // The filter matches terminals and worktrees too, not just threads.
            editor.set_placeholder_text("Search…", window, cx);
            editor
        });
        let thread_rename_editor = cx.new(|cx| Editor::single_line(window, cx));
        let group_rename_editor = cx.new(|cx| Editor::single_line(window, cx));
        let worktree_rename_editor = cx.new(|cx| Editor::single_line(window, cx));

        cx.subscribe_in(
            &multi_workspace,
            window,
            |this, multi_workspace, event: &MultiWorkspaceEvent, window, cx| match event {
                MultiWorkspaceEvent::ActiveWorkspaceChanged { .. } => {
                    this.selection = None;
                    this.sync_active_entry_from_active_workspace(cx);
                    this.schedule_update_entries(false, cx);
                    let multi_workspace = multi_workspace.clone();
                    let workspace = multi_workspace.read(cx).workspace().clone();
                    cx.defer_in(window, move |_, window, cx| {
                        if multi_workspace.read(cx).workspace() == &workspace {
                            workspace.update(cx, |workspace, cx| {
                                ade_workspaces::open_connection_workspace(workspace, window, cx);
                            });
                        }
                    });
                }
                MultiWorkspaceEvent::WorkspaceAdded(workspace) => {
                    this.subscribe_to_workspace(workspace, window, cx);
                    this.schedule_update_entries(false, cx);
                    // ZOrca is terminal-first, so a project the user just added
                    // has to land them in a terminal rather than the agent
                    // panel's empty state. Restoring a session also adds
                    // workspaces, hence the flag: only the add the user asked
                    // for opens one. Deferred because the workspace is still
                    // being activated while this event is delivered.
                    let open_terminal = mem::take(&mut this.open_terminal_for_next_workspace);
                    let multi_workspace = multi_workspace.clone();
                    let workspace = workspace.clone();
                    cx.defer_in(window, move |this, window, cx| {
                        // An ssh connection reattaches to the host's daemon
                        // workspace before the terminal-first default applies —
                        // whether the user just opened it or startup restored
                        // it with a serialized layout, which is the case the
                        // fresh-window path never sees. Every add runs the
                        // flow: it refuses non-ssh windows itself, claims each
                        // window exactly once, and waits out a restored
                        // window's still-loading worktrees.
                        let is_active = multi_workspace.read(cx).workspace() == &workspace;
                        if is_active
                            && workspace.update(cx, |workspace, cx| {
                                ade_workspaces::open_connection_workspace(workspace, window, cx)
                            })
                        {
                            return;
                        }
                        // The terminal lands asynchronously and declines to
                        // steal focus from an open modal, so it cannot dismiss
                        // the folder-trust prompt.
                        if is_active
                            && open_terminal
                            && workspace.read(cx).active_pane().read(cx).items_len() == 0
                        {
                            this.create_new_terminal(&workspace, window, cx);
                        }
                    });
                }
                MultiWorkspaceEvent::WorkspaceRemoved(_)
                | MultiWorkspaceEvent::ProjectGroupsChanged => {
                    this.schedule_update_entries(false, cx);
                }
            },
        )
        .detach();

        // A cold-start window builds its MultiWorkspace before its panels, so
        // the workspaces it restored fired WorkspaceAdded before this sidebar
        // existed to hear it. Whatever is already there gets the same
        // reattach pass the event path runs; the flow claims each window once,
        // so a workspace seen both ways still runs once.
        let workspace = multi_workspace.read(cx).workspace().clone();
        cx.defer_in(window, move |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                ade_workspaces::open_connection_workspace(workspace, window, cx);
            });
        });

        cx.subscribe(
            &group_rename_editor,
            |this: &mut Self, _, event: &editor::EditorEvent, cx| match event {
                // Both confirming and clicking away commit the name; there is
                // nothing destructive to undo, and losing the edit silently is
                // worse than keeping it.
                editor::EditorEvent::Blurred => this.commit_workspace_group_rename(cx),
                _ => {}
            },
        )
        .detach();

        cx.subscribe_in(
            &worktree_rename_editor,
            window,
            |this, _, event, window, cx| {
                if let editor::EditorEvent::Blurred = event {
                    this.commit_worktree_rename(window, cx);
                }
            },
        )
        .detach();

        cx.subscribe(&filter_editor, |this: &mut Self, _, event, cx| {
            if let editor::EditorEvent::BufferEdited = event {
                let query = this.filter_editor.read(cx).text(cx);
                if !query.is_empty() {
                    this.selection.take();
                }
                this.schedule_update_entries(!query.is_empty(), cx);
            }
        })
        .detach();

        cx.subscribe_in(
            &thread_rename_editor,
            window,
            |this, title_editor, event, window, cx| {
                this.handle_thread_rename_editor_event(title_editor, event, window, cx);
            },
        )
        .detach();

        cx.observe(
            &TerminalThreadMetadataStore::global(cx),
            |this, _store, cx| {
                this.schedule_update_entries(false, cx);
            },
        )
        .detach();

        let deferred_multi_workspace = multi_workspace.downgrade();
        cx.defer_in(window, move |this, window, cx| {
            if let Some(multi_workspace) = deferred_multi_workspace.upgrade() {
                let workspaces: Vec<_> = multi_workspace.read(cx).workspaces().cloned().collect();
                for workspace in &workspaces {
                    this.subscribe_to_workspace(workspace, window, cx);
                }
            }
            this.refresh_available_worktrees(cx);
            this.schedule_update_entries(false, cx);
        });

        Self {
            multi_workspace: multi_workspace.downgrade(),
            width: DEFAULT_WIDTH,
            focus_handle,
            filter_editor,
            thread_rename_editor,
            list_state: ListState::new(0, gpui::ListAlignment::Top, px(1000.)),
            contents: SidebarContents::default(),
            collapsed_workspace_nodes: HashSet::default(),
            collapsed_projects: HashSet::default(),
            pending_worktree_open: None,
            pending_worktree_deletions: HashSet::default(),
            pending_worktree_renames: HashSet::default(),
            open_terminal_for_next_workspace: false,
            workspace_groups: Vec::new(),
            pinned_worktrees: Vec::new(),
            unread_worktrees: Vec::new(),
            hidden_worktrees: Vec::new(),
            projects_showing_hidden_worktrees: HashSet::default(),
            available_worktrees: HashMap::default(),
            available_worktrees_refresh_id: 0,
            renaming_workspace_group: None,
            group_rename_editor,
            renaming_worktree: None,
            renaming_worktree_name: None,
            worktree_rename_editor,
            selection: None,
            active_entry: None,
            renaming_thread_id: None,
            suppress_next_rename_edit: false,

            terminal_last_accessed: HashMap::new(),
            thread_switcher: None,
            _thread_switcher_subscriptions: Vec::new(),
            pending_thread_activation: None,
            recent_projects_popover_handle: PopoverMenuHandle::default(),
            add_project_popover_handle: PopoverMenuHandle::default(),
            _subscriptions: Vec::new(),
            _draft_editor_observations: Vec::new(),
            update_task: None,
            _daemon_freshness_watch: Self::watch_daemon_freshness(cx),
        }
    }

    /// Keeps the "upgrade host daemon" arrow honest while the window sits
    /// still.
    ///
    /// The arrow is drawn from
    /// [`ade_workspaces::WorkspaceLifecycleService::host_daemon_stale`], which
    /// a background probe writes and only a render reads — so without
    /// this the arrow would appear, or stop being true, on the user's next
    /// unrelated click rather than when the fact changed.
    ///
    /// `try_lifecycle_service` and not the eager one: a sidebar must not open
    /// the workspace registry's database by existing. In the app it is always
    /// there — [`ade_workspaces::init`] brings up the store, which builds the
    /// service, before any window is made — and where it is not, no backend
    /// exists either, so there is no verdict to miss.
    fn watch_daemon_freshness(cx: &mut Context<Self>) -> Task<()> {
        let Some(lifecycle) = ade_workspaces::try_lifecycle_service(cx) else {
            return Task::ready(());
        };
        let changes = lifecycle.watch_daemon_freshness();
        cx.spawn(async move |this, cx| {
            while changes.recv().await.is_ok() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        })
    }

    fn is_group_collapsed(&self, key: &ProjectGroupKey, cx: &App) -> bool {
        self.multi_workspace
            .upgrade()
            .and_then(|mw| {
                mw.read(cx)
                    .group_state_by_key(key)
                    .map(|state| !state.expanded)
            })
            .unwrap_or(false)
    }

    fn set_group_expanded(&self, key: &ProjectGroupKey, expanded: bool, cx: &mut Context<Self>) {
        if let Some(mw) = self.multi_workspace.upgrade() {
            mw.update(cx, |mw, cx| {
                if let Some(state) = mw.group_state_by_key_mut(key) {
                    state.expanded = expanded;
                }
                mw.serialize(cx);
            });
        }
    }

    fn is_active_workspace(&self, workspace: &Entity<Workspace>, cx: &App) -> bool {
        self.multi_workspace
            .upgrade()
            .map_or(false, |mw| mw.read(cx).workspace() == workspace)
    }

    fn subscribe_to_workspace(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project = workspace.read(cx).project().clone();
        if project.read(cx).is_via_collab() {
            return;
        }

        cx.subscribe_in(
            &project,
            window,
            |this, project, event, _window, cx| match event {
                ProjectEvent::WorktreeAdded(_)
                | ProjectEvent::WorktreeRemoved(_)
                | ProjectEvent::WorktreeOrderChanged => {
                    this.schedule_update_entries(false, cx);
                }
                ProjectEvent::WorktreePathsChanged { old_worktree_paths } => {
                    this.move_entry_paths(project, old_worktree_paths, cx);
                    this.schedule_update_entries(false, cx);
                }
                _ => {}
            },
        )
        .detach();

        let git_store = workspace.read(cx).project().read(cx).git_store().clone();
        cx.subscribe_in(
            &git_store,
            window,
            |this, _, event: &project::git_store::GitStoreEvent, _window, cx| {
                // A newly created worktree is untrusted, so its repositories
                // are not scanned by the time the tree is first built and it
                // lands in the no-version-control fallback as a project of its
                // own. RepositoryAdded is what says the scan finished.
                if matches!(
                    event,
                    project::git_store::GitStoreEvent::RepositoryAdded
                        | project::git_store::GitStoreEvent::RepositoryRemoved(_)
                        | project::git_store::GitStoreEvent::RepositoryUpdated(
                            _,
                            project::git_store::RepositoryEvent::GitWorktreeListChanged
                                | project::git_store::RepositoryEvent::HeadChanged,
                            _,
                        )
                ) {
                    this.refresh_available_worktrees(cx);
                    this.schedule_update_entries(false, cx);
                }
            },
        )
        .detach();

        cx.subscribe_in(
            workspace,
            window,
            move |this, _workspace, event: &workspace::Event, _window, cx| match event {
                // The centre pane owns terminals, so its active item is what
                // `active_entry` has to follow.
                workspace::Event::ActiveItemChanged => {
                    this.sync_active_entry_from_active_workspace(cx);
                    this.schedule_update_entries(false, cx);
                }
                _ => {}
            },
        )
        .detach();

        self.observe_docks(workspace, cx);
    }

    fn refresh_available_worktrees(&mut self, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let mut repositories = HashMap::new();
        for workspace in multi_workspace.read(cx).workspaces() {
            let project = workspace.read(cx).project().read(cx);
            let host = project.remote_connection_options(cx);
            for repository in project.repositories(cx).values() {
                let key = workspace_manager::repository_cache_key(
                    &repository.read(cx).common_dir_abs_path,
                    host.as_ref(),
                );
                if repository.read(cx).linked_worktree_path().is_none()
                    || !repositories.contains_key(&key)
                {
                    repositories.insert(key, repository.clone());
                }
            }
        }
        let requests = repositories
            .into_iter()
            .map(|(key, repository)| {
                let request = repository.update(cx, |repository, _| repository.worktrees());
                (key, request)
            })
            .collect::<Vec<_>>();
        self.available_worktrees_refresh_id += 1;
        let refresh_id = self.available_worktrees_refresh_id;

        cx.spawn(async move |this, cx| {
            let mut available_worktrees = HashMap::new();
            for (key, request) in requests {
                match request.await {
                    Ok(Ok(worktrees)) => {
                        available_worktrees.insert(key, worktrees);
                    }
                    Ok(Err(error)) => log::warn!("failed to list Git worktrees: {error:#}"),
                    Err(_) => log::warn!("Git worktree listing was cancelled"),
                }
            }
            this.update(cx, |this, cx| {
                if this.available_worktrees_refresh_id != refresh_id {
                    return;
                }
                this.available_worktrees = available_worktrees;
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn move_entry_paths(
        &mut self,
        project: &Entity<project::Project>,
        old_paths: &WorktreePaths,
        cx: &mut Context<Self>,
    ) {
        if project.read(cx).is_via_collab() {
            return;
        }

        let new_paths = project.read(cx).worktree_paths(cx);
        let old_folder_paths = old_paths.folder_path_list().clone();

        let added_pairs: Vec<_> = new_paths
            .ordered_pairs()
            .filter(|(main, folder)| {
                !old_paths
                    .ordered_pairs()
                    .any(|(old_main, old_folder)| old_main == *main && old_folder == *folder)
            })
            .map(|(m, f)| (m.clone(), f.clone()))
            .collect();

        let new_folder_paths = new_paths.folder_path_list();
        let removed_folder_paths: Vec<PathBuf> = old_folder_paths
            .paths()
            .iter()
            .filter(|p| !new_folder_paths.paths().contains(p))
            .cloned()
            .collect();

        if added_pairs.is_empty() && removed_folder_paths.is_empty() {
            return;
        }

        let remote_connection = project.read(cx).remote_connection_options(cx);
        let moved_folder_paths = old_paths
            .ordered_pairs()
            .filter_map(|(old_main_path, old_folder_path)| {
                if !removed_folder_paths.contains(old_folder_path) {
                    return None;
                }
                let mut destinations = added_pairs
                    .iter()
                    .filter(|(new_main_path, _)| new_main_path == old_main_path);
                let (_, new_folder_path) = destinations.next()?;
                destinations
                    .next()
                    .is_none()
                    .then(|| (old_folder_path.clone(), new_folder_path.clone()))
            })
            .collect::<Vec<_>>();
        let apply_path_changes = |metadata: &mut TerminalThreadMetadata| {
            for (main_path, folder_path) in &added_pairs {
                metadata.worktree_paths.add_path(main_path, folder_path);
            }
            for path in &removed_folder_paths {
                metadata.worktree_paths.remove_folder_path(path);
            }
            if let Some(working_directory) = metadata.working_directory.as_ref()
                && let Some((new_root, suffix)) = moved_folder_paths
                    .iter()
                    .filter_map(|(old_root, new_root)| {
                        working_directory
                            .strip_prefix(old_root)
                            .ok()
                            .map(|suffix| (old_root, new_root, suffix))
                    })
                    .max_by_key(|(old_root, _, _)| old_root.components().count())
                    .map(|(_, new_root, suffix)| (new_root, suffix))
            {
                metadata.working_directory = Some(new_root.join(suffix));
            }
        };
        TerminalThreadMetadataStore::global(cx).update(cx, |store, store_cx| {
            store.change_worktree_paths(
                &old_folder_paths,
                remote_connection.as_ref(),
                &apply_path_changes,
                store_cx,
            );
        });
    }

    /// Points `active_entry` at whatever terminal the centre pane is showing.
    fn sync_active_entry_from_active_workspace(&mut self, cx: &App) {
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        if self.pending_thread_activation.is_some() {
            return;
        }
        let terminal_id = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<terminal_view::TerminalView>())
                .and_then(|view| view.read(cx).terminal_id())
        });
        if let Some(terminal_id) = terminal_id {
            self.active_entry = Some(ActiveEntry::Terminal {
                terminal_id,
                workspace,
            });
        }
    }

    /// Syncs `active_entry` from the agent panel's current state.
    /// Called from `ActiveViewChanged` — the panel has settled into its
    /// new view, so we can safely read it without race conditions.
    ///
    /// Also resolves `pending_thread_activation` when the panel's
    /// active thread matches the pending activation.
    fn observe_docks(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        let docks: Vec<_> = workspace
            .read(cx)
            .all_docks()
            .into_iter()
            .cloned()
            .collect();
        let workspace = workspace.downgrade();
        for dock in docks {
            let workspace = workspace.clone();
            cx.observe(&dock, move |this, _dock, cx| {
                let Some(workspace) = workspace.upgrade() else {
                    return;
                };
                if !this.is_active_workspace(&workspace, cx) {
                    return;
                }

                cx.notify();
            })
            .detach();
        }
    }

    /// Opens a new workspace for a group that has no open workspaces.
    fn open_workspace_for_group(
        &mut self,
        project_group_key: &ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let path_list = project_group_key.path_list().clone();
        let host = project_group_key.host();
        let provisional_key = Some(project_group_key.clone());
        let active_workspace = multi_workspace.read(cx).workspace().clone();
        let modal_workspace = active_workspace.clone();

        let task = multi_workspace.update(cx, |this, cx| {
            this.find_or_create_workspace(
                path_list,
                host,
                provisional_key,
                |options, window, cx| connect_remote(active_workspace, options, window, cx),
                &[],
                None,
                OpenMode::Activate,
                window,
                cx,
            )
        });

        cx.spawn_in(window, async move |_this, cx| {
            let result = task.await;
            remote_connection::dismiss_connection_modal(&modal_workspace, cx);
            result?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn open_workspace_and_create_entry(
        &mut self,
        project_group_key: &ProjectGroupKey,
        target: NewEntryTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let path_list = project_group_key.path_list().clone();
        if matches!(target, NewEntryTarget::TerminalIfCentreEmpty) {
            self.pending_worktree_open = path_list.paths().first().cloned();
            cx.notify();
        }
        let host = project_group_key.host();
        let provisional_key = Some(project_group_key.clone());
        let active_workspace = multi_workspace.read(cx).workspace().clone();
        let modal_workspace = active_workspace.clone();

        let task = multi_workspace.update(cx, |this, cx| {
            this.find_or_create_workspace(
                path_list,
                host,
                provisional_key,
                |options, window, cx| connect_remote(active_workspace, options, window, cx),
                &[],
                None,
                OpenMode::Activate,
                window,
                cx,
            )
        });

        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            // The modal lives on the workspace that was active at click time;
            // opening a different group switches away from it, so nothing else
            // ever finishes it and it greets the next switch back as a
            // perpetual "Starting proxy…".
            remote_connection::dismiss_connection_modal(&modal_workspace, cx);
            this.update(cx, |this, cx| {
                this.pending_worktree_open = None;
                cx.notify();
            })?;
            let workspace = result?;
            this.update_in(cx, |this, window, cx| match target {
                NewEntryTarget::LastCreatedKind => this.create_new_entry(&workspace, window, cx),
                NewEntryTarget::Terminal => this.create_new_terminal(&workspace, window, cx),
                NewEntryTarget::TerminalIfCentreEmpty => {
                    if workspace.read(cx).active_pane().read(cx).items_len() == 0 {
                        this.create_new_terminal(&workspace, window, cx);
                    }
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Rebuilds the sidebar contents from current workspace and thread state.
    ///
    /// Iterates [`MultiWorkspace::project_group_keys`] to determine project
    /// groups, then populates thread entries from the metadata store and
    /// merges live thread info from active agent panels.
    ///
    /// Aim for a single forward pass over workspaces and threads plus an
    /// O(T log T) sort. Avoid adding extra scans over the data.
    ///
    /// Properties:
    ///
    /// - Should always show every workspace in the multiworkspace
    ///     - If you have no threads, and two workspaces for the worktree and the main workspace, make sure at least one is shown
    /// - Should always show every thread, associated with each workspace in the multiworkspace
    /// - After every build_contents, our "active" state should exactly match the current workspace's, current agent panel's current thread.
    fn rebuild_contents(&mut self, cx: &App) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let mw = multi_workspace.read(cx);
        let workspaces: Vec<_> = mw.workspaces().cloned().collect();
        let active_workspace = Some(mw.workspace().clone());

        let query = self.filter_editor.read(cx).text(cx);

        let _previous = mem::take(&mut self.contents);

        let mut entries = Vec::new();
        let mut notified_terminals: HashSet<TerminalId> = HashSet::new();
        let mut current_terminal_ids: HashSet<TerminalId> = HashSet::new();
        let mut project_header_indices: Vec<usize> = Vec::new();
        let mut seen_terminal_ids: HashSet<TerminalId> = HashSet::new();

        let groups = mw.project_groups(cx);
        let mut live_notified_terminal_ids: HashSet<TerminalId> = HashSet::new();
        for workspace in &workspaces {
            let workspace = workspace.read(cx);
            for pane in workspace.panes() {
                live_notified_terminal_ids.extend(
                    pane.read(cx)
                        .items_of_type::<terminal_view::TerminalView>()
                        .filter_map(|view| {
                            let view = view.read(cx);
                            view.has_bell().then(|| view.terminal_id()).flatten()
                        }),
                );
            }
        }

        let mut all_paths: Vec<PathBuf> = groups
            .iter()
            .flat_map(|group| group.key.path_list().paths().iter().cloned())
            .collect();
        all_paths.sort_unstable();
        all_paths.dedup();
        let path_details =
            util::disambiguate::compute_disambiguation_details(&all_paths, |path, detail| {
                project::path_suffix(path, detail)
            });
        let path_detail_map: HashMap<PathBuf, usize> =
            all_paths.into_iter().zip(path_details).collect();

        let mut branch_by_path: HashMap<PathBuf, SharedString> = HashMap::new();
        for ws in &workspaces {
            let project = ws.read(cx).project().read(cx);
            for project in project.repositories(cx).values() {
                let snapshot = project.read(cx).snapshot();
                if let Some(branch) = &snapshot.branch {
                    branch_by_path.insert(
                        snapshot.work_directory_abs_path.to_path_buf(),
                        SharedString::from(Arc::<str>::from(branch.name())),
                    );
                }
                for linked_wt in snapshot.linked_worktrees() {
                    if let Some(branch) = linked_wt.branch_name() {
                        branch_by_path.insert(
                            linked_wt.path.clone(),
                            SharedString::from(Arc::<str>::from(branch)),
                        );
                    }
                }
            }
        }

        for group in &groups {
            let group_key = &group.key;
            let group_workspaces = &group.workspaces;

            let workspace_by_path_list: HashMap<PathList, &Entity<Workspace>> = group_workspaces
                .iter()
                .map(|ws| (workspace_path_list(ws, cx), ws))
                .collect();
            let resolve_workspace = |folder_paths: &PathList| -> ThreadEntryWorkspace {
                workspace_by_path_list
                    .get(folder_paths)
                    .map(|ws| ThreadEntryWorkspace::Open((*ws).clone()))
                    .unwrap_or_else(|| ThreadEntryWorkspace::Closed {
                        folder_paths: folder_paths.clone(),
                        project_group_key: group_key.clone(),
                    })
            };
            let linked_worktree_path_lists =
                linked_worktree_path_lists_for_workspaces(group_workspaces, cx);
            let make_terminal_entry =
                |metadata: TerminalThreadMetadata, workspace: ThreadEntryWorkspace| {
                    let worktrees =
                        worktree_info_from_thread_paths(&metadata.worktree_paths, &branch_by_path);
                    let has_notification =
                        live_notified_terminal_ids.contains(&metadata.terminal_id);
                    TerminalEntry {
                        metadata,
                        workspace,
                        worktrees,
                        has_notification,
                        highlight_positions: Vec::new(),
                    }
                };

            let mut terminals = Vec::new();
            let terminal_store = TerminalThreadMetadataStore::global(cx);
            let group_host = group_key.host();
            let mut push_terminal_metadata =
                |metadata: TerminalThreadMetadata, workspace: ThreadEntryWorkspace| {
                    if !seen_terminal_ids.insert(metadata.terminal_id) {
                        return;
                    }
                    terminals.push(make_terminal_entry(metadata, workspace));
                };
            for row in terminal_store
                .read(cx)
                .entries_for_main_worktree_path(group_key.path_list(), group_host.as_ref())
                .cloned()
            {
                let workspace = resolve_workspace(row.folder_paths());
                push_terminal_metadata(row, workspace);
            }
            for row in terminal_store
                .read(cx)
                .entries_for_path(group_key.path_list(), group_host.as_ref())
                .cloned()
            {
                let workspace = resolve_workspace(row.folder_paths());
                push_terminal_metadata(row, workspace);
            }
            for ws in group_workspaces {
                let ws_paths = workspace_path_list(ws, cx);
                if ws_paths.paths().is_empty() {
                    continue;
                }
                for row in terminal_store
                    .read(cx)
                    .entries_for_path(&ws_paths, group_host.as_ref())
                    .cloned()
                {
                    push_terminal_metadata(row, ThreadEntryWorkspace::Open(ws.clone()));
                }
            }
            for worktree_path_list in &linked_worktree_path_lists {
                for row in terminal_store
                    .read(cx)
                    .entries_for_path(worktree_path_list, group_host.as_ref())
                    .cloned()
                {
                    push_terminal_metadata(
                        row,
                        ThreadEntryWorkspace::Closed {
                            folder_paths: worktree_path_list.clone(),
                            project_group_key: group_key.clone(),
                        },
                    );
                }
            }
            current_terminal_ids.extend(
                terminals
                    .iter()
                    .map(|terminal| terminal.metadata.terminal_id),
            );
            notified_terminals.extend(terminals.iter().filter_map(|terminal| {
                terminal
                    .has_notification
                    .then_some(terminal.metadata.terminal_id)
            }));
            if group_key.path_list().paths().is_empty() {
                continue;
            }

            let label = group_key.display_name(&path_detail_map);

            let is_collapsed = self.is_group_collapsed(group_key, cx);
            let _should_load_threads = !is_collapsed || !query.is_empty();

            let _is_active = active_workspace
                .as_ref()
                .is_some_and(|active| group_workspaces.contains(active));

            let _group_host = group_key.host();

            let has_entries = !terminals.is_empty();

            if !query.is_empty() {
                let workspace_highlight_positions =
                    fuzzy_match_positions(&query, &label).unwrap_or_default();
                let workspace_matched = !workspace_highlight_positions.is_empty();

                let mut matched_terminals: Vec<TerminalEntry> = Vec::new();
                for mut terminal in terminals {
                    let mut terminal_matched = false;
                    let terminal_title = terminal.metadata.display_title();
                    if let Some(positions) = fuzzy_match_positions(&query, terminal_title.as_ref())
                    {
                        terminal.highlight_positions = positions;
                        terminal_matched = true;
                    }
                    let mut worktree_matched = false;
                    for worktree in &mut terminal.worktrees {
                        let Some(name) = worktree.worktree_name.as_ref() else {
                            continue;
                        };
                        if let Some(positions) = fuzzy_match_positions(&query, name) {
                            worktree.highlight_positions = positions;
                            worktree_matched = true;
                        }
                    }
                    if workspace_matched || terminal_matched || worktree_matched {
                        matched_terminals.push(terminal);
                    }
                }

                if matched_terminals.is_empty() && !workspace_matched {
                    continue;
                }

                project_header_indices.push(entries.len());
                entries.push(ListEntry::ProjectHeader {
                    key: group_key.clone(),
                    label,
                    has_entries,
                });

                Self::push_entries_by_display_time(&mut entries, matched_terminals);
            } else {
                project_header_indices.push(entries.len());
                entries.push(ListEntry::ProjectHeader {
                    key: group_key.clone(),
                    label,
                    has_entries,
                });

                if is_collapsed {
                    continue;
                }

                Self::push_entries_by_display_time(&mut entries, terminals);
            }
        }

        self.terminal_last_accessed
            .retain(|id, _| current_terminal_ids.contains(id));

        self.contents = SidebarContents {
            entries,
            notified_terminals,
            project_header_indices,
        };
    }

    fn schedule_update_entries(&mut self, select_first_after_update: bool, cx: &mut Context<Self>) {
        if self.update_task.is_some() && !select_first_after_update {
            return;
        }

        self.update_task = Some(cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| {
                this.update_task = None;
                this.update_entries(cx);
                if select_first_after_update {
                    this.select_first_entry();
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Rebuilds the sidebar's visible entries from already-cached state.
    fn update_entries(&mut self, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        if !multi_workspace.read(cx).multi_workspace_enabled(cx) {
            return;
        }

        let had_notifications = self.has_notifications(cx);
        let previous_shapes: Vec<EntryShape> =
            self.entry_shapes(multi_workspace.read(cx)).collect();

        self.rebuild_contents(cx);

        // Preserve measurements for unchanged entries so sticky headers do not flicker.
        self.apply_list_state_diff(&previous_shapes, multi_workspace.read(cx));

        if had_notifications != self.has_notifications(cx) {
            multi_workspace.update(cx, |_, cx| {
                cx.notify();
            });
        }

        cx.notify();
    }

    /// Splices only the changed entry range, leaving unchanged item measurements intact.
    fn apply_list_state_diff(
        &self,
        previous_shapes: &[EntryShape],
        multi_workspace: &MultiWorkspace,
    ) {
        let mut new_iter = self.entry_shapes(multi_workspace);
        let mut prefix_len = 0;
        let leading_new = loop {
            match (previous_shapes.get(prefix_len), new_iter.next()) {
                (Some(prev), Some(next)) if *prev == next => prefix_len += 1,
                (None, None) => return,
                (_, leading) => break leading,
            }
        };

        let new_tail: Vec<EntryShape> = leading_new.into_iter().chain(new_iter).collect();
        let prev_tail = &previous_shapes[prefix_len..];
        let suffix_len = prev_tail
            .iter()
            .rev()
            .zip(new_tail.iter().rev())
            .take_while(|(prev, next)| prev == next)
            .count();

        let old_changed = prefix_len..previous_shapes.len() - suffix_len;
        let new_changed_count = new_tail.len() - suffix_len;
        self.list_state.splice(old_changed, new_changed_count);
    }

    fn entry_shapes<'a>(
        &'a self,
        multi_workspace: &'a MultiWorkspace,
    ) -> impl Iterator<Item = EntryShape> + 'a {
        self.contents.entries.iter().map(move |entry| match entry {
            ListEntry::ProjectHeader {
                key, has_entries, ..
            } => EntryShape::ProjectHeader {
                key: key.clone(),
                has_entries: *has_entries,
                is_collapsed: multi_workspace
                    .group_state_by_key(key)
                    .map(|state| !state.expanded)
                    .unwrap_or(false),
            },
            ListEntry::Terminal(terminal) => EntryShape::Terminal(terminal.metadata.terminal_id),
        })
    }

    fn select_first_entry(&mut self) {
        self.selection = self
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::Terminal(_)))
            .or_else(|| {
                if self.contents.entries.is_empty() {
                    None
                } else {
                    Some(0)
                }
            });
    }

    fn dispatch_context(&self, window: &Window, cx: &Context<Self>) -> KeyContext {
        let mut dispatch_context = KeyContext::new_with_defaults();
        dispatch_context.add("ThreadsSidebar");
        dispatch_context.add("menu");

        let is_renaming_thread = self
            .thread_rename_editor
            .focus_handle(cx)
            .is_focused(window);

        let identifier = if self.filter_editor.focus_handle(cx).is_focused(window) {
            "searching"
        } else if is_renaming_thread {
            "editing"
        } else {
            "not_searching"
        };

        dispatch_context.add(identifier);
        dispatch_context
    }

    fn focus_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_handle.is_focused(window) {
            return;
        }

        if self.selection.is_none() {
            self.filter_editor.focus_handle(cx).focus(window, cx);
        }
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.renaming_thread_id.is_some() {
            self.finish_thread_rename(window, cx);
            return;
        }

        if self.renaming_worktree.take().is_some() {
            self.renaming_worktree_name.take();
            self.focus_handle.focus(window, cx);
            cx.notify();
            return;
        }

        if self.filter_editor.read(cx).is_focused(window) {
            if self.reset_filter_editor_text(window, cx) {
                self.selection = None;
                self.update_entries(cx);
                return;
            }

            if self.selection.is_none() {
                self.select_first_entry();
            }
            if self.selection.is_some() {
                self.focus_handle.focus(window, cx);
                cx.notify();
            }
            return;
        }

        if self.reset_filter_editor_text(window, cx) {
            self.update_entries(cx);
        } else {
            self.selection = None;
            self.filter_editor.focus_handle(cx).focus(window, cx);
            cx.notify();
        }
    }

    fn focus_sidebar_filter(
        &mut self,
        _: &FocusSidebarFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selection = None;
        {
            self.filter_editor.focus_handle(cx).focus(window, cx);
        }

        cx.notify();
    }

    fn reset_filter_editor_text(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.filter_editor.update(cx, |editor, cx| {
            if editor.buffer().read(cx).len(cx).0 > 0 {
                editor.set_text("", window, cx);
                true
            } else {
                false
            }
        })
    }

    fn handle_thread_rename_editor_event(
        &mut self,
        title_editor: &Entity<Editor>,
        event: &editor::EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            editor::EditorEvent::BufferEdited => {
                if self.suppress_next_rename_edit {
                    self.suppress_next_rename_edit = false;
                    return;
                }
                if !title_editor.read(cx).is_focused(window) {
                    return;
                }
                let new_title = title_editor.read(cx).text(cx);
                if new_title.is_empty() {
                    return;
                }
                let Some(_thread_id) = self.renaming_thread_id else {
                    return;
                };
            }
            editor::EditorEvent::Blurred => {
                self.finish_thread_rename(window, cx);
            }
            _ => {}
        }
    }

    fn finish_thread_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.renaming_thread_id.take().is_none() {
            return false;
        }
        self.focus_handle.focus(window, cx);
        self.update_entries(cx);
        true
    }

    fn editor_move_down(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        self.select_next(&SelectNext, window, cx);
        if self.selection.is_some() {
            self.focus_handle.focus(window, cx);
        }
    }

    fn editor_move_up(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        self.select_previous(&SelectPrevious, window, cx);
        if self.selection.is_some() {
            self.focus_handle.focus(window, cx);
        }
    }

    fn editor_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_none() {
            self.select_next(&SelectNext, window, cx);
        }
        if self.selection.is_some() {
            self.focus_handle.focus(window, cx);
        }
    }

    fn select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        let row_count = self.visible_row_count(cx);
        let next = match self.selection {
            Some(ix) if ix + 1 < row_count => ix + 1,
            Some(_) if row_count > 0 => 0,
            None if row_count > 0 => 0,
            _ => return,
        };
        self.selection = Some(next);
        self.list_state.scroll_to_reveal_item(next);
        cx.notify();
    }

    fn select_previous(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        match self.selection {
            Some(0) => {
                self.selection = None;
                self.filter_editor.focus_handle(cx).focus(window, cx);
                cx.notify();
            }
            Some(ix) => {
                self.selection = Some(ix - 1);
                self.list_state.scroll_to_reveal_item(ix - 1);
                cx.notify();
            }
            None => {
                if let Some(last) = self.visible_row_count(cx).checked_sub(1) {
                    self.selection = Some(last);
                    self.list_state.scroll_to_reveal_item(last);
                    cx.notify();
                }
            }
        }
    }

    fn select_first(&mut self, _: &SelectFirst, _window: &mut Window, cx: &mut Context<Self>) {
        if self.visible_row_count(cx) > 0 {
            self.selection = Some(0);
            self.list_state.scroll_to_reveal_item(0);
            cx.notify();
        }
    }

    fn select_last(&mut self, _: &SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(last) = self.visible_row_count(cx).checked_sub(1) {
            self.selection = Some(last);
            self.list_state.scroll_to_reveal_item(last);
            cx.notify();
        }
    }

    /// Closes the selected terminal. Still bound to `ArchiveSelectedThread`
    /// because that is the action users' keymaps carry; only the thread half
    /// of its behaviour is gone.
    fn archive_selected_thread(
        &mut self,
        _: &ArchiveSelectedThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.selection else {
            return;
        };
        if let Some(ListEntry::Terminal(terminal)) = self.contents.entries.get(ix) {
            let metadata = terminal.metadata.clone();
            let workspace = terminal.workspace.clone();
            self.close_terminal(&metadata, &workspace, window, cx);
        }
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.finish_thread_rename(window, cx) {
            return;
        }
        if self.renaming_worktree.is_some() {
            self.commit_worktree_rename(window, cx);
            return;
        }

        let Some(ix) = self.selection else { return };
        let tree = self.workspace_tree(cx);
        let rows = tree.rows();
        let Some(row_kind) = rows.get(ix).map(|row| row.kind) else {
            return;
        };
        self.activate_tree_row(&tree, row_kind, window, cx);
    }

    fn expand_selected_entry(
        &mut self,
        _: &SelectChild,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.selection else { return };

        match self.contents.entries.get(ix) {
            Some(ListEntry::ProjectHeader { key, .. }) => {
                let key = key.clone();
                if self.is_group_collapsed(&key, cx) {
                    self.set_group_expanded(&key, true, cx);
                    self.update_entries(cx);
                } else if ix + 1 < self.contents.entries.len() {
                    self.selection = Some(ix + 1);
                    self.list_state.scroll_to_reveal_item(ix + 1);
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    fn collapse_selected_entry(
        &mut self,
        _: &SelectParent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.selection else { return };

        match self.contents.entries.get(ix) {
            Some(ListEntry::ProjectHeader { key, .. }) => {
                let key = key.clone();
                if !self.is_group_collapsed(&key, cx) {
                    self.set_group_expanded(&key, false, cx);
                    self.update_entries(cx);
                }
            }
            Some(ListEntry::Terminal(_)) => {
                for i in (0..ix).rev() {
                    if let Some(ListEntry::ProjectHeader { key, .. }) = self.contents.entries.get(i)
                    {
                        let key = key.clone();
                        self.selection = Some(i);
                        self.set_group_expanded(&key, false, cx);
                        self.update_entries(cx);
                        break;
                    }
                }
            }
            None => {}
        }
    }

    fn toggle_selected_fold(
        &mut self,
        _: &editor::actions::ToggleFold,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.selection else { return };

        // Find the group header for the current selection.
        let header_ix = match self.contents.entries.get(ix) {
            Some(ListEntry::ProjectHeader { .. }) => Some(ix),
            Some(ListEntry::Terminal(_)) => (0..ix).rev().find(|&i| {
                matches!(
                    self.contents.entries.get(i),
                    Some(ListEntry::ProjectHeader { .. })
                )
            }),
            None => None,
        };

        if let Some(header_ix) = header_ix {
            if let Some(ListEntry::ProjectHeader { key, .. }) = self.contents.entries.get(header_ix)
            {
                let key = key.clone();
                if self.is_group_collapsed(&key, cx) {
                    self.set_group_expanded(&key, true, cx);
                } else {
                    self.selection = Some(header_ix);
                    self.set_group_expanded(&key, false, cx);
                }
                self.update_entries(cx);
            }
        }
    }

    fn fold_all(
        &mut self,
        _: &editor::actions::FoldAll,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(mw) = self.multi_workspace.upgrade() {
            mw.update(cx, |mw, cx| {
                mw.set_all_groups_expanded(false);
                // `expanded` is persisted state; folding all must survive a restart
                // just like the per-group toggle in `set_group_expanded`.
                mw.serialize(cx);
            });
        }
        self.update_entries(cx);
    }

    fn unfold_all(
        &mut self,
        _: &editor::actions::UnfoldAll,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(mw) = self.multi_workspace.upgrade() {
            mw.update(cx, |mw, cx| {
                mw.set_all_groups_expanded(true);
                // Same as `fold_all`: `expanded` is persisted state.
                mw.serialize(cx);
            });
        }
        self.update_entries(cx);
    }

    /// Find the neighbor thread in the sidebar (by display position).
    /// Look below first, then above, for the nearest thread that isn't
    /// the one being archived. We capture both the neighbor's metadata
    /// (for activation) and its workspace paths (for the workspace
    /// removal fallback).
    fn neighboring_activatable_entry(&self, current_position: usize) -> Option<ActivatableEntry> {
        let after = self
            .contents
            .entries
            .get(current_position.checked_add(1)?..)?;
        let before = self.contents.entries.get(..current_position)?;
        after
            .iter()
            .chain(before.iter().rev())
            .find_map(ActivatableEntry::from_list_entry)
    }

    fn activate_entry(
        &mut self,
        entry: &ActivatableEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match entry {
            ActivatableEntry::Terminal {
                metadata,
                workspace,
            } => {
                self.activate_terminal_entry(
                    metadata.clone(),
                    workspace.clone(),
                    false,
                    window,
                    cx,
                );
                true
            }
        }
    }

    fn activate_terminal_entry(
        &mut self,
        metadata: TerminalThreadMetadata,
        workspace: ThreadEntryWorkspace,
        retain: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match workspace {
            ThreadEntryWorkspace::Open(workspace) => {
                self.activate_terminal_in_workspace(&workspace, metadata, retain, window, cx);
            }
            ThreadEntryWorkspace::Closed {
                folder_paths,
                project_group_key,
            } => {
                self.open_workspace_and_activate_terminal(
                    metadata,
                    folder_paths,
                    &project_group_key,
                    window,
                    cx,
                );
            }
        }
    }

    /// Brings a stored terminal to the front of the centre pane.
    ///
    /// Activates the live view when the terminal is still open; otherwise
    /// respawns a shell in the directory it was recorded against, tagged with
    /// the same id so the next activation finds it. A terminal's PTY does not
    /// outlive the process, so respawning is what "restoring" one means.
    fn load_agent_terminal_in_workspace(
        workspace: &Entity<Workspace>,
        metadata: &TerminalThreadMetadata,
        focus: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let terminal_id = metadata.terminal_id;
        let recorded_directory = metadata.working_directory.clone();
        workspace.update(cx, |workspace, cx| {
            if terminal_view::terminal_panel::TerminalPanel::activate_center_terminal(
                workspace,
                terminal_id,
                focus,
                window,
                cx,
            ) {
                return;
            }

            // Only an explicit activation opens a terminal. Previewing one in
            // the switcher must not spawn a shell: the new item takes focus,
            // which dismisses the switcher being previewed from.
            if !focus {
                return;
            }

            let working_directory = recorded_directory
                .or_else(|| terminal_view::default_working_directory(workspace, cx));
            terminal_view::terminal_panel::TerminalPanel::add_center_terminal_with_id(
                workspace,
                Some(terminal_id),
                window,
                cx,
                move |project, cx| project.create_terminal_shell(working_directory, cx),
            )
            .detach_and_log_err(cx);
        });
    }

    fn activate_terminal_in_workspace(
        &mut self,
        workspace: &Entity<Workspace>,
        metadata: TerminalThreadMetadata,
        retain: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let terminal_id = metadata.terminal_id;
        self.record_terminal_access(terminal_id);
        self.active_entry = Some(ActiveEntry::Terminal {
            terminal_id,
            workspace: workspace.clone(),
        });

        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.activate(workspace.clone(), None, window, cx);
            if retain {
                multi_workspace.retain_active_workspace(cx);
            }
        });

        Self::load_agent_terminal_in_workspace(workspace, &metadata, true, window, cx);

        self.update_entries(cx);
    }

    fn open_workspace_and_activate_terminal(
        &mut self,
        metadata: TerminalThreadMetadata,
        folder_paths: PathList,
        project_group_key: &ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let host = project_group_key.host();
        let provisional_key = Some(project_group_key.clone());
        let active_workspace = multi_workspace.read(cx).workspace().clone();
        let modal_workspace = active_workspace.clone();

        let open_task = multi_workspace.update(cx, |this, cx| {
            this.find_or_create_workspace(
                folder_paths,
                host,
                provisional_key,
                |options, window, cx| connect_remote(active_workspace, options, window, cx),
                &[],
                None,
                OpenMode::Activate,
                window,
                cx,
            )
        });

        cx.spawn_in(window, async move |this, cx| {
            let result = open_task.await;
            remote_connection::dismiss_connection_modal(&modal_workspace, cx);
            let workspace = result?;
            this.update_in(cx, |this, window, cx| {
                this.activate_terminal_in_workspace(&workspace, metadata, false, window, cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn should_load_closed_workspace_for_archive(
        &self,
        folder_paths: &PathList,
        project_group_key: &ProjectGroupKey,
        remote_connection: Option<&RemoteConnectionOptions>,
        except_terminal_id: Option<TerminalId>,
        cx: &App,
    ) -> bool {
        if folder_paths.is_empty() || folder_paths == project_group_key.path_list() {
            return false;
        }

        TerminalThreadMetadataStore::try_global(cx).is_none_or(|terminal_store| {
            let terminal_store = terminal_store.read(cx);
            !folder_paths.ordered_paths().any(|path| {
                terminal_store.path_is_referenced_by_terminal(
                    except_terminal_id,
                    path,
                    remote_connection,
                )
            })
        })
    }

    fn archive_workspaces(&self, cx: &App) -> Vec<Entity<Workspace>> {
        let multi_workspace = self.multi_workspace.upgrade();
        thread_worktree_archive::workspaces_for_archive(multi_workspace.as_ref(), cx)
    }

    fn roots_to_archive_for_paths(
        &self,
        folder_paths: &PathList,
        remote_connection: Option<&RemoteConnectionOptions>,
        except_terminal_id: Option<TerminalId>,
        cx: &App,
    ) -> Vec<thread_worktree_archive::RootPlan> {
        let workspaces = self.archive_workspaces(cx);
        folder_paths
            .ordered_paths()
            .filter_map(|path| {
                thread_worktree_archive::build_root_plan(path, remote_connection, &workspaces, cx)
            })
            .filter(|root| {
                TerminalThreadMetadataStore::try_global(cx).is_none_or(|terminal_store| {
                    !terminal_store.read(cx).path_is_referenced_by_terminal(
                        except_terminal_id,
                        root.root_path.as_path(),
                        remote_connection,
                    )
                })
            })
            .collect()
    }

    fn linked_worktree_workspace_to_remove(
        &self,
        folder_paths: &PathList,
        remote_connection: Option<&RemoteConnectionOptions>,
        except_terminal_id: Option<TerminalId>,
        roots_to_archive: &[thread_worktree_archive::RootPlan],
        cx: &App,
    ) -> Option<Entity<Workspace>> {
        if folder_paths.is_empty() {
            return None;
        }

        let multi_workspace = self.multi_workspace.upgrade()?;
        let workspace =
            multi_workspace
                .read(cx)
                .workspace_for_paths(folder_paths, remote_connection, cx)?;

        if workspace_has_terminal_metadata_except(&workspace, except_terminal_id, cx) {
            return None;
        }

        if !roots_to_archive.is_empty() {
            let archive_paths: HashSet<&Path> = roots_to_archive
                .iter()
                .map(|root| root.root_path.as_path())
                .collect();
            let project = workspace.read(cx).project().clone();
            let visible_worktree_paths = project
                .read(cx)
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path())
                .collect::<Vec<_>>();
            return (!visible_worktree_paths.is_empty()
                && visible_worktree_paths
                    .iter()
                    .all(|path| archive_paths.contains(path.as_ref())))
            .then_some(workspace);
        }

        let group_key = workspace.read(cx).project_group_key(cx);
        (group_key.path_list() != folder_paths).then_some(workspace)
    }

    async fn wait_for_archive_workspace_metadata(
        workspace: &Entity<Workspace>,
        cx: &mut gpui::AsyncApp,
    ) {
        let scans_complete =
            workspace.read_with(cx, |workspace, cx| workspace.worktree_scans_complete(cx));
        scans_complete.await;

        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
        let barriers = project.update(cx, |project, cx| {
            let repositories = project
                .repositories(cx)
                .values()
                .cloned()
                .collect::<Vec<_>>();
            repositories
                .into_iter()
                .map(|repository| repository.update(cx, |repository, _| repository.barrier()))
                .collect::<Vec<_>>()
        });
        for barrier in barriers {
            let result: anyhow::Result<()> = barrier.await.map_err(|_| {
                anyhow::anyhow!("git repository barrier canceled while archiving worktree")
            });
            result.log_err();
        }
    }

    fn open_workspace_for_archive(
        &mut self,
        folder_paths: PathList,
        project_group_key: ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(Task<anyhow::Result<Entity<Workspace>>>, Entity<Workspace>)> {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return None;
        };

        let host = project_group_key.host();
        let active_workspace = multi_workspace.read(cx).workspace().clone();
        let modal_workspace = active_workspace.clone();

        let open_task = multi_workspace.update(cx, |this, cx| {
            this.find_or_create_workspace(
                folder_paths,
                host,
                Some(project_group_key),
                |options, window, cx| connect_remote(active_workspace, options, window, cx),
                &[],
                None,
                OpenMode::Add,
                window,
                cx,
            )
        });

        Some((open_task, modal_workspace))
    }

    fn open_workspace_and_close_terminal(
        &mut self,
        metadata: TerminalThreadMetadata,
        folder_paths: PathList,
        project_group_key: ProjectGroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((open_task, modal_workspace)) =
            self.open_workspace_for_archive(folder_paths, project_group_key, window, cx)
        else {
            return;
        };

        cx.spawn_in(window, async move |this, cx| {
            let result = open_task.await;
            remote_connection::dismiss_connection_modal(&modal_workspace, cx);
            let workspace = result?;
            Self::wait_for_archive_workspace_metadata(&workspace, cx).await;

            this.update_in(cx, |this, window, cx| {
                let workspace = ThreadEntryWorkspace::Open(workspace);
                this.close_terminal(&metadata, &workspace, window, cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn close_terminal(
        &mut self,
        metadata: &TerminalThreadMetadata,
        workspace: &ThreadEntryWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let ThreadEntryWorkspace::Closed {
            folder_paths,
            project_group_key,
        } = workspace
            && self.should_load_closed_workspace_for_archive(
                folder_paths,
                project_group_key,
                metadata.remote_connection.as_ref(),
                Some(metadata.terminal_id),
                cx,
            )
        {
            self.open_workspace_and_close_terminal(
                metadata.clone(),
                folder_paths.clone(),
                project_group_key.clone(),
                window,
                cx,
            );
            return;
        }

        let terminal_id = metadata.terminal_id;
        let is_active = self
            .active_entry
            .as_ref()
            .is_some_and(|entry| entry.is_active_terminal(terminal_id));
        let neighbor = self
            .contents
            .entries
            .iter()
            .position(|entry| {
                matches!(
                    entry,
                    ListEntry::Terminal(terminal)
                        if terminal.metadata.terminal_id == terminal_id
                )
            })
            .and_then(|position| self.neighboring_activatable_entry(position));

        let terminal_folder_paths = metadata.folder_paths().clone();
        let roots_to_archive = self.roots_to_archive_for_paths(
            metadata.folder_paths(),
            metadata.remote_connection.as_ref(),
            Some(terminal_id),
            cx,
        );

        let workspace_to_remove = self.linked_worktree_workspace_to_remove(
            &terminal_folder_paths,
            metadata.remote_connection.as_ref(),
            Some(terminal_id),
            &roots_to_archive,
            cx,
        );

        let mut workspaces_to_remove: Vec<Entity<Workspace>> =
            workspace_to_remove.into_iter().collect();
        let close_item_tasks = self.close_items_for_archived_worktrees(
            &roots_to_archive,
            &mut workspaces_to_remove,
            window,
            cx,
        );

        if !workspaces_to_remove.is_empty() {
            let multi_workspace = self.multi_workspace.upgrade().unwrap();
            let _terminal_workspace_removed = matches!(
                workspace,
                ThreadEntryWorkspace::Open(workspace) if workspaces_to_remove.contains(workspace)
            );
            let (fallback_paths, project_group_key) = neighbor
                .as_ref()
                .map(|neighbor| neighbor.project_location(cx))
                .unwrap_or_else(|| {
                    workspaces_to_remove
                        .first()
                        .map(|workspace| {
                            let key = workspace.read(cx).project_group_key(cx);
                            (key.path_list().clone(), key)
                        })
                        .unwrap_or_default()
                });

            let excluded = workspaces_to_remove.clone();
            let remove_task = multi_workspace.update(cx, |multi_workspace, cx| {
                multi_workspace.remove(
                    workspaces_to_remove,
                    move |this, window, cx| {
                        let active_workspace = this.workspace().clone();
                        this.find_or_create_workspace(
                            fallback_paths,
                            project_group_key.host(),
                            Some(project_group_key),
                            |options, window, cx| {
                                connect_remote(active_workspace, options, window, cx)
                            },
                            &excluded,
                            None,
                            OpenMode::Activate,
                            window,
                            cx,
                        )
                    },
                    window,
                    cx,
                )
            });

            let metadata = metadata.clone();
            let workspace = workspace.clone();
            cx.spawn_in(window, async move |this, cx| {
                if !remove_task.await? {
                    return anyhow::Ok(());
                }

                for task in close_item_tasks {
                    let result: anyhow::Result<()> = task.await;
                    result.log_err();
                }

                this.update_in(cx, |this, window, cx| {
                    this.close_terminal_entry(
                        &metadata,
                        &workspace,
                        is_active,
                        neighbor.as_ref(),
                        roots_to_archive,
                        window,
                        cx,
                    );
                })?;
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        } else if !close_item_tasks.is_empty() {
            let metadata = metadata.clone();
            let workspace = workspace.clone();
            cx.spawn_in(window, async move |this, cx| {
                for task in close_item_tasks {
                    let result: anyhow::Result<()> = task.await;
                    result.log_err();
                }

                this.update_in(cx, |this, window, cx| {
                    this.close_terminal_entry(
                        &metadata,
                        &workspace,
                        is_active,
                        neighbor.as_ref(),
                        roots_to_archive,
                        window,
                        cx,
                    );
                })?;
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        } else {
            self.close_terminal_entry(
                metadata,
                workspace,
                is_active,
                neighbor.as_ref(),
                roots_to_archive,
                window,
                cx,
            );
        }
    }

    fn close_terminal_entry(
        &mut self,
        metadata: &TerminalThreadMetadata,
        workspace: &ThreadEntryWorkspace,
        is_active: bool,
        neighbor: Option<&ActivatableEntry>,
        roots_to_archive: Vec<thread_worktree_archive::RootPlan>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let terminal_id = metadata.terminal_id;

        if let ThreadEntryWorkspace::Open(workspace) = workspace {
            workspace.update(cx, |workspace, cx| {
                terminal_view::terminal_panel::TerminalPanel::close_center_terminal(
                    workspace,
                    terminal_id,
                    window,
                    cx,
                );
            });
        }
        if let Some(store) = TerminalThreadMetadataStore::try_global(cx) {
            store.update(cx, |store, cx| {
                store.delete(terminal_id, cx);
            });
        }

        self.start_detached_archive_worktree_task(roots_to_archive, cx);

        if is_active {
            self.active_entry = None;
            if neighbor
                .as_ref()
                .is_some_and(|neighbor| self.activate_entry(neighbor, window, cx))
            {
                return;
            }
            self.sync_active_entry_from_active_workspace(cx);
        }
        self.update_entries(cx);
    }

    fn close_items_for_archived_worktrees(
        &self,
        roots_to_archive: &[thread_worktree_archive::RootPlan],
        workspaces_to_remove: &mut Vec<Entity<Workspace>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Task<anyhow::Result<()>>> {
        if roots_to_archive.is_empty() {
            return Vec::new();
        }

        let archive_paths: HashSet<&Path> = roots_to_archive
            .iter()
            .map(|root| root.root_path.as_path())
            .collect();

        let mut mixed_workspaces: Vec<(Entity<Workspace>, Vec<WorktreeId>)> = Vec::new();

        if let Some(multi_workspace) = self.multi_workspace.upgrade() {
            let all_workspaces: Vec<_> = multi_workspace.read(cx).workspaces().cloned().collect();

            for workspace in all_workspaces {
                if workspaces_to_remove.contains(&workspace) {
                    continue;
                }

                let project = workspace.read(cx).project().read(cx);
                let visible_worktrees: Vec<_> = project
                    .visible_worktrees(cx)
                    .map(|worktree| (worktree.read(cx).id(), worktree.read(cx).abs_path()))
                    .collect();

                let archived_worktree_ids: Vec<WorktreeId> = visible_worktrees
                    .iter()
                    .filter(|(_, path)| archive_paths.contains(path.as_ref()))
                    .map(|(id, _)| *id)
                    .collect();

                if archived_worktree_ids.is_empty() {
                    continue;
                }

                if visible_worktrees.len() == archived_worktree_ids.len() {
                    workspaces_to_remove.push(workspace);
                } else {
                    mixed_workspaces.push((workspace, archived_worktree_ids));
                }
            }
        }

        let mut close_item_tasks = Vec::new();
        for (workspace, archived_worktree_ids) in &mixed_workspaces {
            let panes: Vec<_> = workspace.read(cx).panes().to_vec();
            for pane in panes {
                let items_to_close: Vec<EntityId> = pane
                    .read(cx)
                    .items()
                    .filter(|item| {
                        item.project_path(cx)
                            .is_some_and(|pp| archived_worktree_ids.contains(&pp.worktree_id))
                    })
                    .map(|item| item.item_id())
                    .collect();

                if !items_to_close.is_empty() {
                    let task = pane.update(cx, |pane, cx| {
                        pane.close_items(window, cx, SaveIntent::Close, &|item_id| {
                            items_to_close.contains(&item_id)
                        })
                    });
                    close_item_tasks.push(task);
                }
            }
        }

        close_item_tasks
    }

    fn start_detached_archive_worktree_task(
        &self,
        roots: Vec<thread_worktree_archive::RootPlan>,
        cx: &mut Context<Self>,
    ) {
        if roots.is_empty() {
            return;
        }

        let (cancel_tx, cancel_rx) = async_channel::bounded::<()>(1);
        cx.spawn(async move |_this, cx| {
            let outcome = Self::archive_worktree_roots(roots, cancel_rx, cx).await;
            drop(cancel_tx);
            match outcome {
                Ok(ArchiveWorktreeOutcome::Success | ArchiveWorktreeOutcome::Cancelled) => {}
                Err(error) => {
                    log::error!("Failed to archive worktree after closing sidebar item: {error:#}");
                }
            }
        })
        .detach();
    }

    async fn archive_worktree_roots(
        roots: Vec<thread_worktree_archive::RootPlan>,
        cancel_rx: async_channel::Receiver<()>,
        cx: &mut gpui::AsyncApp,
    ) -> anyhow::Result<ArchiveWorktreeOutcome> {
        let mut completed_persists: Vec<(i64, thread_worktree_archive::RootPlan)> = Vec::new();

        for root in &roots {
            if cancel_rx.is_closed() {
                for &(id, ref completed_root) in completed_persists.iter().rev() {
                    thread_worktree_archive::rollback_persist(id, completed_root, cx).await;
                }
                return Ok(ArchiveWorktreeOutcome::Cancelled);
            }

            match thread_worktree_archive::persist_worktree_state(root, cx).await {
                Ok(id) => {
                    completed_persists.push((id, root.clone()));
                }
                Err(error) => {
                    for &(id, ref completed_root) in completed_persists.iter().rev() {
                        thread_worktree_archive::rollback_persist(id, completed_root, cx).await;
                    }
                    return Err(error);
                }
            }

            if cancel_rx.is_closed() {
                for &(id, ref completed_root) in completed_persists.iter().rev() {
                    thread_worktree_archive::rollback_persist(id, completed_root, cx).await;
                }
                return Ok(ArchiveWorktreeOutcome::Cancelled);
            }

            if let Err(error) = thread_worktree_archive::remove_root(root.clone(), cx).await {
                if let Some(&(id, ref completed_root)) = completed_persists.last() {
                    if completed_root.root_path == root.root_path {
                        thread_worktree_archive::rollback_persist(id, completed_root, cx).await;
                        completed_persists.pop();
                    }
                }
                for &(id, ref completed_root) in completed_persists.iter().rev() {
                    thread_worktree_archive::rollback_persist(id, completed_root, cx).await;
                }
                return Err(error);
            }
        }

        Ok(ArchiveWorktreeOutcome::Success)
    }

    fn activate_workspace(
        &self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(multi_workspace) = self.multi_workspace.upgrade() {
            multi_workspace.update(cx, |mw, cx| {
                mw.activate(workspace.clone(), None, window, cx);
            });
        }
    }

    fn record_terminal_access(&mut self, id: TerminalId) {
        self.terminal_last_accessed.insert(id, Utc::now());
    }

    fn push_entries_by_display_time(entries: &mut Vec<ListEntry>, terminals: Vec<TerminalEntry>) {
        fn display_time(entry: &ListEntry) -> DateTime<Utc> {
            match entry {
                ListEntry::Terminal(terminal) => terminal.metadata.created_at,
                ListEntry::ProjectHeader { .. } => unreachable!(),
            }
        }

        let row_entries = terminals
            .into_iter()
            .map(ListEntry::Terminal)
            .sorted_by_key(|right| std::cmp::Reverse(display_time(right)));

        entries.extend(row_entries);
    }

    /// The sort order used by the ctrl-tab switcher
    fn switcher_entry_cmp(
        &self,
        left: &ThreadSwitcherEntry,
        right: &ThreadSwitcherEntry,
    ) -> Ordering {
        let sort_time = |entry: &ThreadSwitcherEntry| match entry {
            ThreadSwitcherEntry::Terminal(entry) => self
                .terminal_last_accessed
                .get(&entry.metadata.terminal_id)
                .copied()
                .unwrap_or(entry.metadata.created_at),
        };

        // .reverse() = most recent first
        sort_time(left).cmp(&sort_time(right)).reverse()
    }

    fn mru_entries_for_switcher(&self, _cx: &App) -> Vec<ThreadSwitcherEntry> {
        let mut current_header_label: Option<SharedString> = None;
        let mut current_header_key: Option<ProjectGroupKey> = None;
        let mut entries: Vec<ThreadSwitcherEntry> = self
            .contents
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::ProjectHeader { label, key, .. } => {
                    current_header_label = Some(label.clone());
                    current_header_key = Some(key.clone());
                    None
                }
                ListEntry::Terminal(terminal) => {
                    let timestamp: SharedString =
                        format_history_entry_timestamp(terminal.metadata.created_at).into();
                    Some(ThreadSwitcherEntry::Terminal(ThreadSwitcherTerminalEntry {
                        metadata: terminal.metadata.clone(),
                        workspace: terminal.workspace.clone(),
                        project_name: current_header_label.clone(),
                        worktrees: terminal
                            .worktrees
                            .iter()
                            .cloned()
                            .map(|mut wt| {
                                wt.highlight_positions = Vec::new();
                                wt
                            })
                            .collect(),
                        notified: self
                            .contents
                            .is_terminal_notified(terminal.metadata.terminal_id),
                        timestamp,
                    }))
                }
            })
            .collect();

        entries.sort_by(|a, b| self.switcher_entry_cmp(a, b));

        entries
    }

    fn dismiss_thread_switcher(&mut self, cx: &mut Context<Self>) {
        self.thread_switcher = None;
        self._thread_switcher_subscriptions.clear();
        if let Some(mw) = self.multi_workspace.upgrade() {
            mw.update(cx, |mw, cx| {
                mw.set_sidebar_overlay(None, cx);
            });
        }
    }

    fn on_toggle_thread_switcher(
        &mut self,
        action: &ToggleThreadSwitcher,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_thread_switcher_impl(action.select_last, window, cx);
    }

    fn preview_switcher_selection(
        &mut self,
        selection: &ThreadSwitcherSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match selection {
            ThreadSwitcherSelection::Terminal {
                metadata,
                workspace,
            } => {
                if let ThreadEntryWorkspace::Open(workspace) = workspace {
                    if let Some(multi_workspace) = self.multi_workspace.upgrade() {
                        multi_workspace.update(cx, |multi_workspace, cx| {
                            multi_workspace.activate(workspace.clone(), None, window, cx);
                        });
                    }
                    self.active_entry = Some(ActiveEntry::Terminal {
                        terminal_id: metadata.terminal_id,
                        workspace: workspace.clone(),
                    });
                    self.update_entries(cx);
                    Self::load_agent_terminal_in_workspace(workspace, metadata, false, window, cx);
                }
            }
        }
    }

    fn confirm_switcher_selection(
        &mut self,
        selection: &ThreadSwitcherSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match selection {
            ThreadSwitcherSelection::Terminal {
                metadata,
                workspace,
            } => {
                self.dismiss_thread_switcher(cx);
                self.activate_terminal_entry(metadata.clone(), workspace.clone(), true, window, cx);
            }
        }
    }

    fn toggle_thread_switcher_impl(
        &mut self,
        select_last: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(thread_switcher) = &self.thread_switcher {
            thread_switcher.update(cx, |switcher, cx| {
                if select_last {
                    switcher.select_last(cx);
                } else {
                    switcher.cycle_selection(cx);
                }
            });
            return;
        }

        let entries = self.mru_entries_for_switcher(cx);
        if entries.len() < 2 {
            return;
        }

        let weak_multi_workspace = self.multi_workspace.clone();

        // Snapshot the active entry (thread or terminal) so dismissal can
        // restore it.
        let original_active_entry = self.active_entry.clone();
        let original_workspace = self
            .multi_workspace
            .upgrade()
            .map(|mw| mw.read(cx).workspace().clone());

        let thread_switcher = cx.new(|cx| ThreadSwitcher::new(entries, select_last, window, cx));

        let mut subscriptions = Vec::new();

        subscriptions.push(cx.subscribe_in(&thread_switcher, window, {
            let thread_switcher = thread_switcher.clone();
            move |this, _emitter, event: &ThreadSwitcherEvent, window, cx| match event {
                ThreadSwitcherEvent::Preview(selection) => {
                    this.preview_switcher_selection(selection, window, cx);
                    let focus = thread_switcher.focus_handle(cx);
                    window.focus(&focus, cx);
                }
                ThreadSwitcherEvent::Confirmed(selection) => {
                    this.confirm_switcher_selection(selection, window, cx);
                }
                ThreadSwitcherEvent::Dismissed => {
                    if let Some(mw) = weak_multi_workspace.upgrade() {
                        if let Some(original_ws) = &original_workspace {
                            mw.update(cx, |mw, cx| {
                                mw.activate(original_ws.clone(), None, window, cx);
                            });
                        }
                    }
                    match &original_active_entry {
                        Some(ActiveEntry::Terminal {
                            terminal_id,
                            workspace,
                        }) => {
                            let terminal_id = *terminal_id;
                            let workspace = workspace.clone();
                            this.active_entry = Some(ActiveEntry::Terminal {
                                terminal_id,
                                workspace: workspace.clone(),
                            });
                            this.update_entries(cx);
                            workspace.update(cx, |workspace, cx| {
                                terminal_view::terminal_panel::TerminalPanel::activate_center_terminal(
                                    workspace,
                                    terminal_id,
                                    false,
                                    window,
                                    cx,
                                );
                            });
                        }
                        None => {}
                    }
                    this.dismiss_thread_switcher(cx);
                }
            }
        }));

        subscriptions.push(cx.subscribe_in(
            &thread_switcher,
            window,
            |this, _emitter, _event: &gpui::DismissEvent, _window, cx| {
                this.dismiss_thread_switcher(cx);
            },
        ));

        let focus = thread_switcher.focus_handle(cx);
        let overlay_view = gpui::AnyView::from(thread_switcher.clone());

        // Replay the initial preview that was emitted during construction
        // before subscriptions were wired up.
        let initial_preview = thread_switcher
            .read(cx)
            .selected_entry()
            .map(ThreadSwitcherEntry::selection);

        self.thread_switcher = Some(thread_switcher);
        self._thread_switcher_subscriptions = subscriptions;
        if let Some(mw) = self.multi_workspace.upgrade() {
            mw.update(cx, |mw, cx| {
                mw.set_sidebar_overlay(Some(overlay_view), cx);
            });
        }

        if let Some(selection) = initial_preview {
            self.preview_switcher_selection(&selection, window, cx);
        }

        window.focus(&focus, cx);
    }

    fn render_filter_input(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .min_w_0()
            .flex_1()
            .capture_action(
                cx.listener(|this, _: &editor::actions::Newline, window, cx| {
                    this.editor_confirm(window, cx);
                }),
            )
            .child(self.filter_editor.clone())
    }

    /// The "Add Project" control: a recent-projects picker that can open a
    /// local folder or connect to a remote host. Both the footer button and the
    /// one in the "Projects" header are this same menu, so adding a project
    /// offers the same choices wherever it is started from.
    fn render_add_project_button(
        &self,
        element_id: &'static str,
        button_id: &'static str,
        icon: IconName,
        anchor: gpui::Anchor,
        offset: gpui::Point<Pixels>,
        popover_handle: PopoverMenuHandle<SidebarRecentProjects>,
        // Whether a project added through this button should land the user in a
        // terminal, as ZOrca's terminal-first flow expects.
        opens_terminal: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let multi_workspace = self.multi_workspace.upgrade();
        let this = cx.entity().downgrade();

        let workspace = multi_workspace
            .as_ref()
            .map(|mw| mw.read(cx).workspace().downgrade());

        let focus_handle = workspace
            .as_ref()
            .and_then(|ws| ws.upgrade())
            .map(|w| w.read(cx).focus_handle(cx))
            .unwrap_or_else(|| cx.focus_handle());

        let window_project_groups: Vec<ProjectGroupKey> = multi_workspace
            .as_ref()
            .map(|mw| mw.read(cx).project_group_keys())
            .unwrap_or_default();

        PopoverMenu::new(element_id)
            .with_handle(popover_handle)
            .menu(move |window, cx| {
                if opens_terminal {
                    // Armed on open rather than on pick, matching what this
                    // button did when it opened the native folder picker: a
                    // dismissed picker left the flag set too.
                    this.update(cx, |this, _| {
                        this.open_terminal_for_next_workspace = true;
                    })
                    .log_err();
                }
                workspace.as_ref().map(|ws| {
                    SidebarRecentProjects::popover(
                        ws.clone(),
                        window_project_groups.clone(),
                        focus_handle.clone(),
                        window,
                        cx,
                    )
                })
            })
            .trigger_with_tooltip(
                IconButton::new(button_id, icon)
                    .icon_size(IconSize::Small)
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent)),
                |_window, cx| Tooltip::for_action("Add Project", &OpenRecent::default(), cx),
            )
            .offset(offset)
            .anchor(anchor)
    }

    fn render_recent_projects_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_add_project_button(
            "sidebar-recent-projects-menu",
            "open-project",
            IconName::FolderAdd,
            gpui::Anchor::BottomRight,
            gpui::Point {
                x: px(-2.0),
                y: px(-2.0),
            },
            self.recent_projects_popover_handle.clone(),
            false,
            cx,
        )
    }

    fn new_thread_in_group(
        &mut self,
        _: &NewThreadInGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(key) = self.selected_group_key() {
            self.set_group_expanded(&key, true, cx);
            self.selection = None;
            if let Some(workspace) = self.workspace_for_group(&key, cx) {
                self.create_new_entry(&workspace, window, cx);
            } else {
                self.open_workspace_and_create_entry(
                    &key,
                    NewEntryTarget::LastCreatedKind,
                    window,
                    cx,
                );
            }
        } else if let Some(workspace) = self.active_workspace(cx) {
            self.create_new_entry(&workspace, window, cx);
        }
    }

    fn new_terminal_thread(
        &mut self,
        _: &NewTerminalThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();

        if let Some(key) = self.selected_group_key() {
            self.set_group_expanded(&key, true, cx);
            self.selection = None;
            if let Some(workspace) = self.workspace_for_group(&key, cx) {
                self.create_new_terminal(&workspace, window, cx);
            } else {
                self.open_workspace_and_create_entry(&key, NewEntryTarget::Terminal, window, cx);
            }
        } else if let Some(workspace) = self.active_workspace(cx) {
            self.create_new_terminal(&workspace, window, cx);
        }
    }

    fn create_new_entry(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if workspace_path_list(workspace, cx).paths().is_empty() {
            return;
        }

        // A project that cannot host a terminal (e.g. a collab guest) gets
        // nothing: there is no other kind of entry left to create.
        if self.should_create_terminal_for_workspace(workspace, cx) {
            self.create_new_terminal(workspace, window, cx);
        }
    }

    fn should_create_terminal_for_workspace(
        &self,
        workspace: &Entity<Workspace>,
        cx: &App,
    ) -> bool {
        // The panel used to answer this, because the choice was thread or
        // terminal. A terminal is the only kind of entry now, so the only
        // question left is whether the project can host one.
        workspace.read(cx).project().read(cx).supports_terminal(cx)
    }

    fn create_new_terminal(
        &mut self,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.create_new_terminal_with_command(workspace, None, window, cx);
    }

    fn create_new_terminal_with_command(
        &mut self,
        workspace: &Entity<Workspace>,
        command: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if workspace_path_list(workspace, cx).paths().is_empty() {
            return;
        }

        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        // `activate` re-focuses even when the workspace is already active, and
        // that focus change dismisses any open modal — which is how creating
        // the terminal was closing the folder-trust prompt.
        if multi_workspace.read(cx).workspace() != workspace {
            multi_workspace.update(cx, |multi_workspace, cx| {
                multi_workspace.activate(workspace.clone(), None, window, cx);
            });
        }

        // A fresh SSH workspace is still empty while ADE attaches its
        // persistent terminal. Let that claimed flow supply the first tab;
        // otherwise this stock shell races it and both appear.
        if command.is_none()
            && !workspace.read(cx).ade_owns_layout()
            && workspace.update(cx, |workspace, cx| {
                ade_workspaces::open_connection_workspace(workspace, window, cx)
            })
        {
            return;
        }

        let Some(command) = command else {
            // Call the center path directly rather than dispatching
            // `NewTerminal`: the sidebar is a `MultiWorkspace` sibling of the
            // `Workspace`, so a dispatched action resolves against whatever
            // happens to be focused — which is why a project with an agent
            // panel got a dock terminal and one without got a center tab.
            workspace.update(cx, |workspace, cx| {
                let working_directory = terminal_view::default_working_directory(workspace, cx);
                terminal_view::terminal_panel::TerminalPanel::add_center_terminal(
                    workspace,
                    window,
                    cx,
                    move |project, cx| project.create_terminal_shell(working_directory, cx),
                )
                .detach_and_log_err(cx);
            });
            return;
        };

        // Agent presets go to the center too. The init command is delivered by
        // writing to the terminal after it starts, not by a creation-time hook,
        // so the center path can carry it.
        let command = command.to_owned();
        workspace.update(cx, |workspace, cx| {
            let working_directory = terminal_view::default_working_directory(workspace, cx);
            let terminal = terminal_view::terminal_panel::TerminalPanel::add_center_terminal(
                workspace,
                window,
                cx,
                move |project, cx| project.create_terminal_shell(working_directory, cx),
            );
            cx.spawn(async move |_, cx| {
                let terminal = terminal.await?;
                let Some(terminal) = terminal.upgrade() else {
                    return anyhow::Ok(());
                };
                cx.update(|cx| {
                    agent_workspaces::write_terminal_init_command(&terminal, command, cx)
                });
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        });
    }

    fn selected_group_key(&self) -> Option<ProjectGroupKey> {
        let ix = self.selection?;
        match self.contents.entries.get(ix) {
            Some(ListEntry::ProjectHeader { key, .. }) => Some(key.clone()),
            Some(ListEntry::Terminal(_)) => {
                (0..ix)
                    .rev()
                    .find_map(|i| match self.contents.entries.get(i) {
                        Some(ListEntry::ProjectHeader { key, .. }) => Some(key.clone()),
                        _ => None,
                    })
            }
            _ => None,
        }
    }

    fn workspace_for_group(&self, key: &ProjectGroupKey, cx: &App) -> Option<Entity<Workspace>> {
        let mw = self.multi_workspace.upgrade()?;
        let mw = mw.read(cx);
        let active = mw.workspace().clone();
        let active_key = active.read(cx).project_group_key(cx);
        if active_key.matches(key) {
            Some(active)
        } else {
            mw.workspace_for_paths(key.path_list(), key.host().as_ref(), cx)
        }
    }

    fn active_project_group_key(&self, cx: &App) -> Option<ProjectGroupKey> {
        let multi_workspace = self.multi_workspace.upgrade()?;
        let multi_workspace = multi_workspace.read(cx);
        Some(multi_workspace.project_group_key_for_workspace(multi_workspace.workspace(), cx))
    }

    fn active_project_header_position(&self, cx: &App) -> Option<usize> {
        let active_key = self.active_project_group_key(cx)?;
        self.contents
            .project_header_indices
            .iter()
            .position(|&entry_ix| {
                matches!(
                    &self.contents.entries[entry_ix],
                    ListEntry::ProjectHeader { key, .. } if key.matches(&active_key)
                )
            })
    }

    fn cycle_project_impl(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };

        let header_count = self.contents.project_header_indices.len();
        if header_count == 0 {
            return;
        }

        let current_pos = self.active_project_header_position(cx);

        let next_pos = match current_pos {
            Some(pos) => {
                if forward {
                    (pos + 1) % header_count
                } else {
                    (pos + header_count - 1) % header_count
                }
            }
            None => 0,
        };

        let header_entry_ix = self.contents.project_header_indices[next_pos];
        let Some(ListEntry::ProjectHeader { key, .. }) = self.contents.entries.get(header_entry_ix)
        else {
            return;
        };
        let key = key.clone();

        // Uncollapse the target group so that threads become visible.
        self.set_group_expanded(&key, true, cx);

        if let Some(workspace) = self.multi_workspace.upgrade().and_then(|mw| {
            mw.read(cx)
                .workspace_for_paths(key.path_list(), key.host().as_ref(), cx)
        }) {
            multi_workspace.update(cx, |multi_workspace, cx| {
                multi_workspace.activate(workspace, None, window, cx);
                multi_workspace.retain_active_workspace(cx);
            });
        } else {
            self.open_workspace_for_group(&key, window, cx);
        }
    }

    fn on_next_project(&mut self, _: &NextProject, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_project_impl(true, window, cx);
    }

    fn on_previous_project(
        &mut self,
        _: &PreviousProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_project_impl(false, window, cx);
    }

    fn render_command_palette_button(&self) -> impl IntoElement {
        div()
            .debug_selector(|| "sidebar-command-palette".to_owned())
            .child(
                IconButton::new("sidebar-command-palette", IconName::ListCollapse)
                    .icon_size(IconSize::Small)
                    .tab_index(0isize)
                    .aria_label("Command Palette")
                    .tooltip(|_window, cx| {
                        Tooltip::for_action(
                            "Command Palette",
                            &zed_actions::command_palette::Toggle,
                            cx,
                        )
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(zed_actions::command_palette::Toggle), cx);
                    }),
            )
    }

    /// The gear and question-mark buttons Orca carries at the bottom of its
    /// sidebar. Both mirror entries that already exist elsewhere (the title
    /// bar's menu and the Help app menu) rather than introducing new ones, so
    /// there is one place per command and the keybindings stay accurate.
    fn render_settings_button(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        PopoverMenu::new("sidebar-settings-menu")
            .menu(move |window, cx| {
                Some(ContextMenu::build(window, cx, |menu, _, _cx| {
                    menu.action("Settings", Box::new(zed_actions::OpenSettings))
                        .action("Keymap", Box::new(zed_actions::OpenKeymap))
                        .action(
                            "Themes\u{2026}",
                            Box::new(zed_actions::theme_selector::Toggle::default()),
                        )
                        .action(
                            "Icon Themes\u{2026}",
                            Box::new(zed_actions::icon_theme_selector::Toggle::default()),
                        )
                        .action("Extensions", Box::new(zed_actions::Extensions::default()))
                }))
            })
            .trigger_with_tooltip(
                IconButton::new("sidebar-settings", IconName::Settings).icon_size(IconSize::Small),
                |_window, cx| Tooltip::for_action("Settings", &zed_actions::OpenSettings, cx),
            )
            .anchor(gpui::Anchor::BottomLeft)
    }

    fn render_help_button(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        PopoverMenu::new("sidebar-help-menu")
            .menu(move |window, cx| {
                Some(ContextMenu::build(window, cx, |menu, _, _cx| {
                    menu.action("Documentation", Box::new(zed_actions::OpenDocs))
                        .separator()
                        .action("View Telemetry", Box::new(zed_actions::OpenTelemetryLog))
                        .action(
                            "View Dependency Licenses",
                            Box::new(zed_actions::OpenLicenses),
                        )
                        .separator()
                        .action("About", Box::new(zed_actions::About))
                }))
            })
            .trigger_with_tooltip(
                IconButton::new("sidebar-help", IconName::CircleHelp).icon_size(IconSize::Small),
                |window, cx| Tooltip::text("Help")(window, cx),
            )
            .anchor(gpui::Anchor::BottomLeft)
    }

    fn render_sidebar_bottom_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let on_right = self.side(cx) == SidebarSide::Right;

        h_flex()
            .p_1()
            .gap_1()
            .when(on_right, |this| this.flex_row_reverse())
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(self.render_settings_button(cx))
            .child(self.render_help_button(cx))
            .child(self.render_command_palette_button())
            .child(div().flex_1())
            .child(self.render_recent_projects_button(cx))
    }

    fn active_workspace(&self, cx: &App) -> Option<Entity<Workspace>> {
        self.multi_workspace
            .upgrade()
            .map(|w| w.read(cx).workspace().clone())
    }
}

impl WorkspaceSidebar for Sidebar {
    fn width(&self, _cx: &App) -> Pixels {
        self.width
    }

    fn set_width(&mut self, width: Option<Pixels>, cx: &mut Context<Self>) {
        self.width = width.unwrap_or(DEFAULT_WIDTH).clamp(MIN_WIDTH, MAX_WIDTH);
        cx.notify();
    }

    fn has_notifications(&self, _cx: &App) -> bool {
        !self.contents.notified_terminals.is_empty()
    }

    fn is_threads_list_view_active(&self) -> bool {
        true
    }

    fn side(&self, cx: &App) -> SidebarSide {
        AgentSettings::get_global(cx).sidebar_side()
    }

    fn prepare_for_focus(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.selection = None;
        cx.notify();
    }

    fn toggle_thread_switcher(
        &mut self,
        select_last: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_thread_switcher_impl(select_last, window, cx);
    }

    fn cycle_project(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_project_impl(forward, window, cx);
    }

    fn serialized_state(&self, _cx: &App) -> Option<String> {
        let serialized = SerializedSidebar {
            width: Some(f32::from(self.width)),
            workspace_groups: self.workspace_groups.clone(),
            pinned_worktrees: self.pinned_worktrees.clone(),
            unread_worktrees: self.unread_worktrees.clone(),
            hidden_worktrees: self.hidden_worktrees.clone(),
            collapsed_projects: self.collapsed_projects.iter().cloned().collect(),
            collapsed_workspace_nodes: self
                .collapsed_workspace_nodes
                .iter()
                .map(|key| key.to_string())
                .sorted()
                .collect(),
        };
        serde_json::to_string(&serialized).ok()
    }

    fn restore_serialized_state(
        &mut self,
        state: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(serialized) = serde_json::from_str::<SerializedSidebar>(state).log_err() {
            if let Some(width) = serialized.width {
                self.width = px(width).clamp(MIN_WIDTH, MAX_WIDTH);
            }
            self.workspace_groups = serialized.workspace_groups;
            self.pinned_worktrees = serialized.pinned_worktrees;
            self.unread_worktrees = serialized.unread_worktrees;
            self.hidden_worktrees = serialized.hidden_worktrees;
            self.collapsed_projects = serialized.collapsed_projects.into_iter().collect();
            self.collapsed_workspace_nodes = serialized
                .collapsed_workspace_nodes
                .into_iter()
                .map(SharedString::from)
                .collect();
        }
        cx.notify();
    }
}

impl gpui::EventEmitter<workspace::SidebarEvent> for Sidebar {}

impl Focusable for Sidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Sidebar {
    fn toggle_workspace_node_collapsed(
        &mut self,
        key: WorkspaceCollapseKey,
        cx: &mut Context<Self>,
    ) {
        match key {
            WorkspaceCollapseKey::Global(key) => {
                if !self.collapsed_workspace_nodes.remove(&key) {
                    self.collapsed_workspace_nodes.insert(key);
                }
            }
            WorkspaceCollapseKey::Project { key, legacy_key } => {
                self.collapsed_workspace_nodes.remove(&legacy_key);
                if !self.collapsed_projects.remove(&key) {
                    self.collapsed_projects.insert(key);
                }
            }
        }
        // Collapse is part of the arrangement the user built, so it has to
        // survive a restart like the groups do.
        cx.emit(workspace::SidebarEvent::SerializeNeeded);
        cx.notify();
    }

    fn toggle_worktree_unread(
        &mut self,
        root: PathBuf,
        host_key: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self
            .unread_worktrees
            .iter()
            .position(|state| state.matches(&root, host_key.as_deref()))
        {
            self.unread_worktrees.remove(index);
        } else {
            self.unread_worktrees
                .push(workspace_manager::ScopedPath::new(root, host_key));
        }
        cx.emit(workspace::SidebarEvent::SerializeNeeded);
        cx.notify();
    }

    fn toggle_worktree_hidden(
        &mut self,
        root: PathBuf,
        project_key: PathBuf,
        host_key: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self
            .hidden_worktrees
            .iter()
            .position(|state| state.matches(&root, host_key.as_deref()))
        {
            self.hidden_worktrees.remove(index);
        } else {
            self.hidden_worktrees
                .push(workspace_manager::ScopedPath::new(root, host_key.clone()));
            self.projects_showing_hidden_worktrees
                .remove(&workspace_manager::ScopedPath::new(project_key, host_key));
        }
        cx.emit(workspace::SidebarEvent::SerializeNeeded);
        cx.notify();
    }

    fn toggle_project_hidden_worktrees(
        &mut self,
        project_key: PathBuf,
        host_key: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let key = workspace_manager::ScopedPath::new(project_key, host_key);
        if !self.projects_showing_hidden_worktrees.remove(&key) {
            self.projects_showing_hidden_worktrees.insert(key);
        }
        cx.notify();
    }

    /// Orca clears the dot when the worktree is activated or its pane touched.
    fn clear_worktree_unread(
        &mut self,
        root: &Path,
        host_key: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .unread_worktrees
            .iter()
            .position(|state| state.matches(root, host_key))
        else {
            return;
        };
        self.unread_worktrees.remove(index);
        cx.emit(workspace::SidebarEvent::SerializeNeeded);
        cx.notify();
    }

    fn toggle_worktree_pinned(
        &mut self,
        root: PathBuf,
        host_key: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self
            .pinned_worktrees
            .iter()
            .position(|state| state.matches(&root, host_key.as_deref()))
        {
            self.pinned_worktrees.remove(index);
        } else {
            self.pinned_worktrees
                .push(workspace_manager::ScopedPath::new(root, host_key));
        }
        cx.emit(workspace::SidebarEvent::SerializeNeeded);
        cx.notify();
    }

    fn workspace_group_assignments(
        &self,
    ) -> Vec<(SharedString, Vec<workspace_manager::ScopedPath>)> {
        self.workspace_groups
            .iter()
            .map(|group| {
                (
                    SharedString::from(group.name.clone()),
                    group.projects.clone(),
                )
            })
            .collect()
    }

    fn workspace_group_index(&self, name: &str) -> Option<usize> {
        self.workspace_groups
            .iter()
            .position(|group| group.name == name)
    }

    /// Names a new group so it never collides with an existing one, which
    /// would make the two indistinguishable in the move-to menu.
    fn unused_workspace_group_name(&self, preferred: &str) -> String {
        if self.workspace_group_index(preferred).is_none() {
            return preferred.to_owned();
        }
        (2..)
            .map(|suffix| format!("{preferred} {suffix}"))
            .find(|name| self.workspace_group_index(name).is_none())
            .unwrap_or_else(|| preferred.to_owned())
    }

    fn create_workspace_group(
        &mut self,
        preferred_name: &str,
        projects: Vec<workspace_manager::ScopedPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = self.unused_workspace_group_name(preferred_name);
        for group in &mut self.workspace_groups {
            group.projects.retain(|path| !projects.contains(path));
        }
        self.workspace_groups
            .push(SerializedWorkspaceGroup { name, projects });
        let index = self.workspace_groups.len() - 1;
        self.workspace_groups_changed(cx);
        self.start_renaming_workspace_group(index, window, cx);
    }

    fn move_project_to_workspace_group(
        &mut self,
        project: workspace_manager::ScopedPath,
        group_index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        for group in &mut self.workspace_groups {
            group.projects.retain(|path| path != &project);
        }
        if let Some(group) = group_index.and_then(|index| self.workspace_groups.get_mut(index)) {
            group.projects.push(project);
        }
        self.workspace_groups_changed(cx);
    }

    /// Deleting a group only ungroups its projects; they return to the top
    /// level rather than disappearing with it.
    fn delete_workspace_group(&mut self, group_index: usize, cx: &mut Context<Self>) {
        if group_index >= self.workspace_groups.len() {
            return;
        }
        self.workspace_groups.remove(group_index);
        if self.renaming_workspace_group == Some(group_index) {
            self.renaming_workspace_group = None;
        }
        self.workspace_groups_changed(cx);
    }

    fn workspace_groups_changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(workspace::SidebarEvent::SerializeNeeded);
        cx.notify();
    }

    fn start_renaming_workspace_group(
        &mut self,
        group_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.workspace_groups.get(group_index) else {
            return;
        };
        let name = group.name.clone();
        self.renaming_workspace_group = Some(group_index);
        self.group_rename_editor.update(cx, |editor, cx| {
            editor.set_text(name, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }

    fn commit_workspace_group_rename(&mut self, cx: &mut Context<Self>) {
        let Some(group_index) = self.renaming_workspace_group.take() else {
            return;
        };
        let name = self.group_rename_editor.read(cx).text(cx).trim().to_owned();
        if !name.is_empty()
            && self
                .workspace_group_index(&name)
                .is_none_or(|existing| existing == group_index)
            && let Some(group) = self.workspace_groups.get_mut(group_index)
        {
            group.name = name;
        }
        self.workspace_groups_changed(cx);
    }

    fn start_renaming_worktree(
        &mut self,
        root: PathBuf,
        host_key: Option<String>,
        repository_key: Option<PathBuf>,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.renaming_worktree = Some((root, host_key, repository_key));
        self.renaming_worktree_name = Some(name.clone());
        self.worktree_rename_editor.update(cx, |editor, cx| {
            editor.set_text(name, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
        });
        let editor = self.worktree_rename_editor.clone();
        cx.defer_in(window, move |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.focus_handle(cx).focus(window, cx);
            });
        });
        cx.notify();
    }

    fn commit_worktree_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((old_path, selected_host_key, selected_repository_key)) =
            self.renaming_worktree.take()
        else {
            return;
        };
        let name = self.worktree_rename_editor.read(cx).text(cx);
        if self
            .renaming_worktree_name
            .take()
            .is_some_and(|old_name| old_name == name.trim())
        {
            cx.notify();
            return;
        }
        let Some(new_path) = renamed_worktree_path(&old_path, &name) else {
            cx.notify();
            return;
        };
        if new_path == old_path {
            cx.notify();
            return;
        }

        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let worktrees_to_update = multi_workspace
            .read(cx)
            .workspaces()
            .flat_map(|workspace| {
                let project = workspace.read(cx).project().clone();
                let workspace_host_key = workspace_manager::host_cache_key(
                    project.read(cx).remote_connection_options(cx).as_ref(),
                );
                if workspace_host_key != selected_host_key {
                    return Vec::new();
                }
                if selected_repository_key.as_ref().is_some_and(|selected| {
                    !project
                        .read(cx)
                        .repositories(cx)
                        .values()
                        .any(|repository| {
                            repository.read(cx).common_dir_abs_path.as_ref() == selected
                        })
                }) {
                    return Vec::new();
                }
                project
                    .read(cx)
                    .visible_worktrees(cx)
                    .filter_map(|worktree| {
                        let worktree = worktree.read(cx);
                        let worktree_path = worktree.abs_path();
                        let suffix = worktree_path.strip_prefix(&old_path).ok()?;
                        Some((project.clone(), worktree.id(), new_path.join(suffix)))
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let old_group_keys = multi_workspace
            .read(cx)
            .workspaces()
            .filter_map(|workspace| {
                let project = workspace.read(cx).project().read(cx);
                let host = project.remote_connection_options(cx);
                (workspace_manager::host_cache_key(host.as_ref()) == selected_host_key
                    && selected_repository_key.as_ref().is_none_or(|selected| {
                        project.repositories(cx).values().any(|repository| {
                            repository.read(cx).common_dir_abs_path.as_ref() == selected
                        })
                    }))
                .then(|| ProjectGroupKey::new(host, PathList::new(std::slice::from_ref(&old_path))))
            })
            .unique()
            .collect::<Vec<_>>();
        let repositories = multi_workspace
            .read(cx)
            .workspaces()
            .flat_map(|workspace| {
                let project = workspace.read(cx).project().read(cx);
                let host_key = workspace_manager::host_cache_key(
                    project.remote_connection_options(cx).as_ref(),
                );
                project
                    .repositories(cx)
                    .values()
                    .cloned()
                    .map(|repository| (host_key.clone(), repository))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let repository = repositories
            .into_iter()
            .filter(|(repository_host_key, repository)| {
                if repository_host_key != &selected_host_key {
                    return false;
                }
                let key = (
                    repository.read(cx).common_dir_abs_path.to_path_buf(),
                    repository_host_key.clone(),
                );
                selected_repository_key
                    .as_ref()
                    .is_none_or(|selected| selected == &key.0)
                    && self.available_worktrees.get(&key).is_some_and(|worktrees| {
                        worktrees.iter().any(|worktree| worktree.path == old_path)
                    })
            })
            .min_by_key(|(_, repository)| repository.read(cx).linked_worktree_path().is_some())
            .map(|(_, repository)| repository);
        let Some(repository) = repository else {
            cx.notify();
            return;
        };
        let repository_key = repository.read(cx).common_dir_abs_path.to_path_buf();
        let pending_rename = (
            old_path.clone(),
            selected_host_key.clone(),
            repository_key.clone(),
        );
        if !self.pending_worktree_renames.insert(pending_rename.clone()) {
            cx.notify();
            return;
        }
        let workspace = self
            .active_workspace(cx)
            .map(|workspace| workspace.downgrade());
        let multi_workspace = multi_workspace.downgrade();

        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let result = repository
                    .update(cx, |repository, _| {
                        repository.rename_worktree(old_path.clone(), new_path.clone())
                    })
                    .await?;

                match result {
                    Ok(()) => {
                        if let Some(multi_workspace) = multi_workspace.upgrade() {
                            multi_workspace.update(cx, |multi_workspace, cx| {
                                for old_group_key in &old_group_keys {
                                    multi_workspace
                                        .remove_project_group_if_empty(old_group_key, cx);
                                }
                            });
                        }
                        for (project, worktree_id, new_worktree_path) in worktrees_to_update {
                            project.update(cx, |project, cx| {
                                project.update_worktree_abs_path(
                                    worktree_id,
                                    &new_worktree_path,
                                    cx,
                                );
                            });
                        }
                        this.update(cx, |this, cx| {
                            update_cached_worktree_path(
                                &mut this.available_worktrees,
                                &repository_key,
                                selected_host_key.as_deref(),
                                &old_path,
                                &new_path,
                            );
                            for paths in [
                                &mut this.pinned_worktrees,
                                &mut this.unread_worktrees,
                                &mut this.hidden_worktrees,
                            ] {
                                for path in paths.iter_mut().filter(|path| {
                                    path.matches(&old_path, selected_host_key.as_deref())
                                }) {
                                    *path = workspace_manager::ScopedPath::new(
                                        new_path.clone(),
                                        selected_host_key.clone(),
                                    );
                                }
                            }
                            this.refresh_available_worktrees(cx);
                            cx.emit(workspace::SidebarEvent::SerializeNeeded);
                            cx.notify();
                        })?;
                    }
                    Err(error) => {
                        if let Some(workspace) = workspace {
                            workspace
                                .update(cx, |workspace, cx| {
                                    workspace.show_toast(
                                        Toast::new(
                                            NotificationId::unique::<RenameWorktree>(),
                                            format!("Unable to rename worktree: {error:#}"),
                                        ),
                                        cx,
                                    );
                                })
                                .ok();
                        }
                    }
                }
                anyhow::Ok(())
            }
            .await;
            this.update(cx, |this, _| {
                this.pending_worktree_renames.remove(&pending_rename);
            })?;
            result
        })
        .detach_and_log_err(cx);
    }

    fn collapse_key(group: Option<&SharedString>, project: Option<&SharedString>) -> SharedString {
        match (group, project) {
            (Some(group), Some(project)) => SharedString::from(format!("{group}/{project}")),
            (Some(group), None) => group.clone(),
            (None, Some(project)) => SharedString::from(format!("/{project}")),
            (None, None) => SharedString::default(),
        }
    }

    fn scoped_project_key(
        path: PathBuf,
        group_key: Option<&ProjectGroupKey>,
    ) -> workspace_manager::ScopedPath {
        workspace_manager::ScopedPath::new(
            path,
            workspace_manager::host_cache_key(group_key.and_then(ProjectGroupKey::host).as_ref()),
        )
    }

    fn visible_state_scopes(
        tree: &workspace_manager::WorkspaceTree,
    ) -> HashSet<(PathBuf, Option<String>)> {
        tree.groups
            .iter()
            .flat_map(|group| &group.projects)
            .flat_map(|project| {
                project.worktrees.iter().flat_map(move |worktree| {
                    let host_key = worktree
                        .group_key
                        .as_ref()
                        .and_then(ProjectGroupKey::host)
                        .as_ref()
                        .and_then(|host| workspace_manager::host_cache_key(Some(host)));
                    std::iter::once((project.key.to_path_buf(), host_key.clone())).chain(
                        worktree
                            .folder_root
                            .iter()
                            .cloned()
                            .map(move |root| (root, host_key.clone())),
                    )
                })
            })
            .collect()
    }

    fn resolve_legacy_state(&mut self, tree: &workspace_manager::WorkspaceTree) -> bool {
        let scopes = Self::visible_state_scopes(tree);
        let resolve = |state: &workspace_manager::ScopedPath| state.resolved(&scopes);
        let mut changed = false;
        let mut migrate = |state: &mut Vec<workspace_manager::ScopedPath>| {
            let migrated: Vec<_> = state.iter().filter_map(resolve).collect();
            changed |= *state != migrated;
            *state = migrated;
        };
        migrate(&mut self.pinned_worktrees);
        migrate(&mut self.unread_worktrees);
        migrate(&mut self.hidden_worktrees);
        let collapsed_projects: HashSet<_> =
            self.collapsed_projects.iter().filter_map(resolve).collect();
        changed |= self.collapsed_projects != collapsed_projects;
        self.collapsed_projects = collapsed_projects;
        for group in &mut self.workspace_groups {
            let migrated: Vec<_> = group.projects.iter().filter_map(resolve).collect();
            changed |= group.projects != migrated;
            group.projects = migrated;
        }
        changed
    }

    /// Builds the tree from the open workspaces and re-applies the collapse
    /// state the user chose, which the rebuild would otherwise discard.
    fn workspace_tree(&mut self, cx: &mut Context<Self>) -> workspace_manager::WorkspaceTree {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return workspace_manager::WorkspaceTree::default();
        };
        let workspaces: Vec<_> = multi_workspace.read(cx).workspaces().cloned().collect();
        // Groups whose workspace is not open — a restored window reopens only
        // its active workspace, and the rest must keep their place in the bar.
        let open_keys: Vec<ProjectGroupKey> = workspaces
            .iter()
            .map(|workspace| workspace.read(cx).project_group_key(cx))
            .collect();
        let closed_groups =
            closed_project_groups(&open_keys, multi_workspace.read(cx).project_group_keys());
        let mut tree = workspace_manager::build_tree(
            &workspaces,
            &self.available_worktrees,
            &closed_groups,
            cx,
        );
        if self.resolve_legacy_state(&tree) {
            cx.emit(workspace::SidebarEvent::SerializeNeeded);
        }
        workspace_manager::apply_groups(&mut tree, &self.workspace_group_assignments());
        workspace_manager::apply_pins(&mut tree, &self.pinned_worktrees);
        workspace_manager::apply_unread(&mut tree, &self.unread_worktrees);
        workspace_manager::apply_hidden_worktrees(
            &mut tree,
            &self.hidden_worktrees.iter().cloned().collect(),
            &self.projects_showing_hidden_worktrees,
        );
        workspace_manager::filter_tree(&mut tree, &self.filter_editor.read(cx).text(cx));

        tree.pinned_collapsed = self
            .collapsed_workspace_nodes
            .contains(&SharedString::from("pinned"));
        let legacy_project_counts = tree
            .groups
            .iter()
            .flat_map(|group| {
                group.projects.iter().map(move |project| {
                    Self::collapse_key(group.name.as_ref(), Some(&project.name))
                })
            })
            .counts();
        for group in &mut tree.groups {
            group.collapsed = self
                .collapsed_workspace_nodes
                .contains(&Self::collapse_key(group.name.as_ref(), None));
            for project in &mut group.projects {
                let legacy_key = Self::collapse_key(group.name.as_ref(), Some(&project.name));
                let project_key = project
                    .worktrees
                    .first()
                    .and_then(|worktree| worktree.group_key.as_ref())
                    .map(|group_key| {
                        Self::scoped_project_key(project.key.to_path_buf(), Some(group_key))
                    });
                if legacy_project_counts.get(&legacy_key) == Some(&1)
                    && self.collapsed_workspace_nodes.remove(&legacy_key)
                    && let Some(project_key) = project_key.clone()
                {
                    self.collapsed_projects.insert(project_key);
                    cx.emit(workspace::SidebarEvent::SerializeNeeded);
                }
                project.collapsed = project_key
                    .as_ref()
                    .is_some_and(|key| self.collapsed_projects.contains(key));
            }
        }

        tree
    }

    /// The collapse key for a row, or `None` for rows that cannot collapse.
    fn collapse_key_for_row(
        tree: &workspace_manager::WorkspaceTree,
        kind: &workspace_manager::RowKind,
    ) -> Option<WorkspaceCollapseKey> {
        use workspace_manager::RowKind;
        match kind {
            RowKind::PinnedSection => Some(WorkspaceCollapseKey::Global("pinned".into())),
            RowKind::Group(id) => tree
                .groups
                .iter()
                .find(|group| group.id == *id)
                .map(|group| {
                    WorkspaceCollapseKey::Global(Self::collapse_key(group.name.as_ref(), None))
                }),
            RowKind::Project(id) => tree.groups.iter().find_map(|group| {
                group
                    .projects
                    .iter()
                    .find(|project| project.id == *id)
                    .and_then(|project| {
                        let group_key = project.worktrees.first()?.group_key.as_ref()?;
                        Some(WorkspaceCollapseKey::Project {
                            key: Self::scoped_project_key(
                                project.key.to_path_buf(),
                                Some(group_key),
                            ),
                            legacy_key: Self::collapse_key(
                                group.name.as_ref(),
                                Some(&project.name),
                            ),
                        })
                    })
            }),
            RowKind::Worktree(_) => None,
        }
    }

    /// Orca reveals a row's actions on hover: an overflow menu on every row,
    /// and on the levels that can hold worktrees a `+` that creates one.
    /// Everything a workspace-manager row's actions need, resolved once so the
    /// hover menu and the right-click menu cannot disagree.
    fn workspace_row_context(
        &self,
        tree: &workspace_manager::WorkspaceTree,
        kind: workspace_manager::RowKind,
    ) -> WorkspaceRowContext {
        use workspace_manager::RowKind;
        let project_key = match kind {
            RowKind::Project(id) => tree.project_key(id).map(|key| key.to_path_buf()),
            _ => None,
        };
        let worktree_root = match kind {
            RowKind::Worktree(id) => tree.worktree_root(id),
            _ => None,
        };
        let workspace_key = tree.group_key_for_row(&kind);
        let removal_keys = match kind {
            RowKind::Project(id) => tree.project_group_keys(id).to_vec(),
            _ => workspace_key.clone().into_iter().collect(),
        };
        let host_key = workspace_manager::host_cache_key(
            workspace_key
                .as_ref()
                .and_then(ProjectGroupKey::host)
                .as_ref(),
        );
        let has_hidden_worktrees = project_key.as_ref().is_some_and(|project_key| {
            let cache_key = workspace_manager::repository_cache_key(
                project_key,
                workspace_key
                    .as_ref()
                    .and_then(ProjectGroupKey::host)
                    .as_ref(),
            );
            self.available_worktrees
                .get(&cache_key)
                .is_some_and(|worktrees| {
                    worktrees.iter().any(|worktree| {
                        self.hidden_worktrees
                            .iter()
                            .any(|state| state.matches(&worktree.path, host_key.as_deref()))
                    })
                })
        });
        WorkspaceRowContext {
            kind,
            host_key: host_key.clone(),
            ade_host: workspace_key
                .as_ref()
                .and_then(|key| key.host())
                .as_ref()
                .and_then(ade_workspaces::destination_for),
            workspace_key,
            removal_keys,
            group_index: match kind {
                RowKind::Group(id) => tree
                    .group_name(id)
                    .and_then(|name| self.workspace_group_index(&name)),
                _ => None,
            },
            project_key: project_key.clone(),
            worktree_project_key: match kind {
                RowKind::Worktree(id) => tree.worktree_project_key(id),
                _ => None,
            },
            removable_worktree: match kind {
                RowKind::Worktree(id) => tree.removable_worktree_root(id),
                _ => None,
            },
            worktree_workspace: match kind {
                RowKind::Worktree(id) => tree.workspace_for(id),
                _ => None,
            },
            can_create_worktree: match kind {
                RowKind::Project(id) => tree.project_has_repository(id),
                _ => false,
            },
            has_hidden_worktrees,
            shows_hidden_worktrees: project_key.as_ref().is_some_and(|key| {
                self.projects_showing_hidden_worktrees
                    .iter()
                    .any(|state| state.matches(key, host_key.as_deref()))
            }),
            worktree_is_hidden: worktree_root.as_ref().is_some_and(|root| {
                self.hidden_worktrees
                    .iter()
                    .any(|state| state.matches(root, host_key.as_deref()))
            }),
            can_hide_worktree: match kind {
                RowKind::Worktree(id) => !tree.worktree_is_primary(id),
                _ => false,
            },
            worktree_root,
            worktree_name: match kind {
                RowKind::Worktree(id) => tree.worktree_name(id),
                _ => None,
            },
        }
    }

    /// Removes a worktree while its repository is still connected, then closes
    /// it using the project group captured before Git changes its path.
    fn delete_worktree(
        &mut self,
        root: PathBuf,
        row_workspace: Option<WeakEntity<Workspace>>,
        row_group_key: Option<ProjectGroupKey>,
        selected_repository_key: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let workspace = row_workspace
            .and_then(|workspace| workspace.upgrade())
            .or_else(|| {
                workspace_for_scoped_root(
                    multi_workspace.read(cx),
                    &root,
                    row_group_key.as_ref(),
                    cx,
                )
            });
        let project_group_key = workspace
            .as_ref()
            .map(|workspace| workspace.read(cx).project_group_key(cx))
            .or(row_group_key);
        if workspace.is_none() && project_group_key.is_none() {
            return;
        }
        let worktree_group_key = project_group_key.as_ref().map(|project_group_key| {
            ProjectGroupKey::new(
                project_group_key.host(),
                PathList::new(std::slice::from_ref(&root)),
            )
        });
        let local_fs = project_group_key
            .as_ref()
            .is_some_and(|key| key.host().is_none())
            .then(|| <dyn fs::Fs>::global(cx));
        let host_key = workspace_manager::host_cache_key(
            project_group_key
                .as_ref()
                .and_then(ProjectGroupKey::host)
                .as_ref(),
        );
        let remaining_project = workspace.as_ref().and_then(|workspace| {
            let project = workspace.read(cx).project().clone();
            let worktrees = project
                .read(cx)
                .visible_worktrees(cx)
                .map(|worktree| {
                    let worktree = worktree.read(cx);
                    (worktree.id(), worktree.abs_path())
                })
                .collect::<Vec<_>>();
            (worktrees.len() > 1)
                .then(|| {
                    worktrees
                        .into_iter()
                        .find(|(_, path)| path.as_ref() == root)
                        .map(|(id, _)| (project, id))
                })
                .flatten()
        });
        let repository = workspace
            .as_ref()
            .and_then(|workspace| {
                workspace
                    .read(cx)
                    .project()
                    .read(cx)
                    .active_repository(cx)
                    .filter(|repository| {
                        let repository = repository.read(cx);
                        selected_repository_key.as_ref().is_none_or(|selected| {
                            repository.common_dir_abs_path.as_ref() == selected
                        }) && (repository
                            .linked_worktree_path()
                            .is_some_and(|path| path.as_ref() == root)
                            || repository
                                .snapshot()
                                .main_worktree_abs_path()
                                .is_some_and(|path| path == root))
                    })
                    .map(|repository| (repository, workspace.downgrade()))
            })
            .or_else(|| {
                let repository_key = self.available_worktrees.iter().find_map(
                    |((key, repository_host), worktrees)| {
                        (*repository_host == host_key
                            && selected_repository_key
                                .as_ref()
                                .is_none_or(|selected| selected == key)
                            && worktrees.iter().any(|worktree| worktree.path == root))
                        .then_some((key, repository_host))
                    },
                )?;
                multi_workspace.read(cx).workspaces().find_map(|workspace| {
                    let project = workspace.read(cx).project().read(cx);
                    let workspace_host = workspace_manager::host_cache_key(
                        project.remote_connection_options(cx).as_ref(),
                    );
                    if workspace_host.as_ref() != repository_key.1.as_ref() {
                        return None;
                    }
                    project
                        .repositories(cx)
                        .values()
                        .find(|repository| {
                            repository.read(cx).common_dir_abs_path.as_ref()
                                == repository_key.0.as_path()
                        })
                        .cloned()
                        .map(|repository| (repository, workspace.downgrade()))
                })
            });
        let display_name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string_lossy().into_owned());
        let deletion_key = workspace_manager::ScopedPath::new(root.clone(), host_key.clone());
        if !self.pending_worktree_deletions.insert(deletion_key.clone()) {
            return;
        }
        cx.notify();

        let confirmation = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("Delete worktree \"{display_name}\"?"),
            Some("The worktree is removed from disk. Its branch is kept."),
            &["Delete", "Cancel"],
            cx,
        );

        cx.spawn_in(window, async move |this, cx| {
            let result = async {
            if confirmation.await != Ok(0) {
                return anyhow::Ok(());
            }

            if repository.is_none() {
                let unresolved_repository = match local_fs {
                    Some(fs) => match fs.metadata(&root).await {
                        Ok(None) => None,
                        Ok(Some(_)) => Some(
                            "ZOrca could not find the Git repository for this worktree, so no files were removed. Open the repository and try again."
                                .to_owned(),
                        ),
                        Err(error) => Some(format!(
                            "ZOrca could not verify whether the worktree still exists: {error:#}"
                        )),
                    },
                    None => Some(
                        "ZOrca could not find the remote Git repository for this worktree, so no files were removed. Open the repository and try again."
                            .to_owned(),
                    ),
                };

                if let Some(detail) = unresolved_repository {
                    let prompt = cx.update(|window, cx| {
                        window.prompt(
                            gpui::PromptLevel::Critical,
                            "Unable to delete worktree",
                            Some(&detail),
                            &["OK"],
                            cx,
                        )
                    })?;
                    prompt.await.log_err();
                    return anyhow::Ok(());
                }
            }

            let removed = if let Some((repository, error_workspace)) = repository {
                let removal = cx.update(|window, cx| {
                    git_ui::worktree_service::remove_worktree(
                        repository,
                        root.clone(),
                        display_name,
                        error_workspace,
                        window,
                        cx,
                    )
                })?;
                removal.await
            } else {
                Ok(true)
            }?;
            if !removed {
                return anyhow::Ok(());
            }

            if let Some((project, worktree_id)) = remaining_project {
                project.update(cx, |project, cx| {
                    project.remove_worktree(worktree_id, cx);
                });
            } else if let Some(workspace) = workspace {
                let Some(project_group_key) = project_group_key else {
                    return anyhow::Ok(());
                };
                let original_group_key = project_group_key.clone();
                let close = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
                    multi_workspace.close_workspace_in_project_group(
                        &workspace,
                        project_group_key,
                        window,
                        cx,
                    )
                })?;
                close.await?;

                let current_group_key =
                    workspace.read_with(cx, |workspace, cx| workspace.project_group_key(cx));
                multi_workspace.update(cx, |multi_workspace, cx| {
                    multi_workspace.remove_project_group_if_empty(&original_group_key, cx);
                    multi_workspace.remove_project_group_if_empty(&current_group_key, cx);
                    if let Some(worktree_group_key) = &worktree_group_key {
                        multi_workspace.remove_project_group_if_empty(worktree_group_key, cx);
                    }
                });
            } else if let Some(project_group_key) = project_group_key {
                multi_workspace.update(cx, |multi_workspace, cx| {
                    multi_workspace.remove_project_group_if_empty(&project_group_key, cx);
                    if let Some(worktree_group_key) = &worktree_group_key {
                        multi_workspace.remove_project_group_if_empty(worktree_group_key, cx);
                    }
                });
            }
            this.update(cx, |this, cx| {
                let host_key = host_key.as_deref();
                let mut state_changed = false;
                for paths in [
                    &mut this.pinned_worktrees,
                    &mut this.unread_worktrees,
                    &mut this.hidden_worktrees,
                ] {
                    let previous_len = paths.len();
                    paths.retain(|path| !path.matches(&root, host_key));
                    state_changed |= paths.len() != previous_len;
                }
                if state_changed {
                    cx.emit(workspace::SidebarEvent::SerializeNeeded);
                }
                this.refresh_available_worktrees(cx);
            })?;
            anyhow::Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                if this.pending_worktree_deletions.remove(&deletion_key) {
                    cx.notify();
                }
            })?;
            result
        })
        .detach_and_log_err(cx);
    }

    /// Orca reveals a row's actions on hover: an overflow menu on every row,
    /// and on a project a `+` that creates a worktree.
    fn render_workspace_manager_row_actions(
        &self,
        ix: usize,
        context: &WorkspaceRowContext,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let can_create_worktree = context.can_create_worktree;
        let workspace_key = context.workspace_key.clone();
        let stale_daemon_host = self.stale_daemon_host(context, cx);

        Some(
            h_flex()
                .gap_px()
                .child(
                    PopoverMenu::new(SharedString::from(format!(
                        "workspace-manager-row-menu-{ix}"
                    )))
                    .trigger(
                        IconButton::new("workspace-manager-row-more", IconName::Ellipsis)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("More Actions")),
                    )
                    .menu({
                        let build = self.workspace_manager_row_menu(context.clone(), cx);
                        move |window, cx| Some(build(window, cx))
                    })
                    .anchor(gpui::Anchor::TopRight),
                )
                .when_some(
                    workspace_key.filter(|_| can_create_worktree),
                    |this, key| {
                        this.child(
                            IconButton::new("workspace-manager-row-new-worktree", IconName::Plus)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("New Worktree"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if let Some(workspace) = this.workspace_for_group(&key, cx) {
                                        this.open_worktree_picker(&workspace, window, cx);
                                    }
                                })),
                        )
                    },
                )
                // Remote rows only, and only where a hash comparison already
                // found the host's daemon behind: an arrow that is always
                // there says "an upgrade is conceivable", not "an update
                // exists". The local daemon is replaced when the app is.
                .when_some(stale_daemon_host, |this, host| {
                    this.child(
                        IconButton::new("workspace-manager-row-upgrade-daemon", IconName::ArrowUp)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Upgrade Host Daemon"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.upgrade_host_daemon(host.clone(), cx);
                            })),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_daemon_upgrade_action(
        &self,
        context: &WorkspaceRowContext,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let host = self.stale_daemon_host(context, cx)?;
        Some(
            IconButton::new(
                "workspace-manager-row-upgrade-daemon-visible",
                IconName::ArrowUp,
            )
            .icon_size(IconSize::Small)
            .tooltip(Tooltip::text("Upgrade Host Daemon"))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.upgrade_host_daemon(host.clone(), cx);
            }))
            .into_any_element(),
        )
    }

    /// This row's host, when something has already found its daemon behind the
    /// binary this client would deploy. `None` for a local row, and for a host
    /// nobody has compared — an unanswered question must not draw an arrow.
    ///
    /// `try_lifecycle_service` and not the eager one: a render must not be what
    /// builds the service, which opens the workspace registry's database on the
    /// way. No service means nothing has contacted any host, which reads the
    /// same as "not stale".
    fn stale_daemon_host(&self, context: &WorkspaceRowContext, cx: &App) -> Option<String> {
        stale_daemon_host_for_row(context.kind, context.ade_host.as_deref(), |host| {
            ade_workspaces::try_lifecycle_service(cx)
                .is_some_and(|lifecycle| lifecycle.host_daemon_stale(host))
        })
    }

    /// Build the current daemon binary and put it on `host`, replacing the one
    /// running there.
    ///
    /// Manual because the automatic upgrade only fires when a connect happens
    /// to catch the host's daemon both stale and holding nothing, which on a
    /// host somebody works on may never happen. The click therefore *forces*
    /// the swap: the daemon exits over whatever it holds, and its sessions come
    /// back as lost rows the reconcile pass recreates. Being told "busy" would
    /// leave the operator with no way through at all.
    ///
    /// Every outcome is a toast: the build alone can take minutes, so silence
    /// would be indistinguishable from a click that never registered.
    fn upgrade_host_daemon(&mut self, host: String, cx: &mut Context<Self>) {
        let lifecycle = ade_workspaces::lifecycle_service(cx);
        // Weak across the await: the upgrade takes minutes, and a strong handle
        // would keep the window's workspace alive long after it closed just to
        // deliver a toast nobody can see.
        let workspace = self.active_workspace(cx).map(|w| w.downgrade());
        cx.spawn(async move |_, cx| {
            let outcome = cx
                .background_spawn({
                    let host = host.clone();
                    // Blocking, and slow: a cross-compile, an ssh round trip
                    // and an upload.
                    async move { lifecycle.upgrade_host_daemon(&host) }
                })
                .await;
            let message = match &outcome {
                Ok(DaemonUpgradeOutcome::Upgraded) => format!("Daemon upgraded on {host}"),
                Ok(DaemonUpgradeOutcome::UpToDate) => {
                    format!("Daemon already up to date on {host}")
                }
                Err(error) => format!("Could not upgrade the daemon on {host}: {error:#}"),
            };
            match &outcome {
                Ok(_) => log::info!("{message}"),
                Err(_) => log::warn!("{message}"),
            }
            let Some(workspace) = workspace else {
                return;
            };
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_toast(
                        Toast::new(NotificationId::unique::<UpgradeHostDaemon>(), message),
                        cx,
                    )
                })
                .ok();
        })
        .detach();
    }

    fn kill_and_recreate_workspace_sessions(
        &mut self,
        workspace: WeakEntity<Workspace>,
        worktree_name: SharedString,
        worktree_root: PathBuf,
        ade_host: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let confirmation = window.prompt(
            gpui::PromptLevel::Critical,
            &format!("Kill and recreate sessions for \"{worktree_name}\"?"),
            Some(
                "This terminates every terminal and agent running in this worktree and deletes their scrollback. Repository files and other worktrees are not changed.",
            ),
            &["Kill and Recreate", "Cancel"],
            cx,
        );

        window
            .spawn(cx, async move |cx| {
                if confirmation.await != Ok(0) {
                    return anyhow::Ok(());
                }
                let Some(workspace) = workspace.upgrade() else {
                    return anyhow::Ok(());
                };
                let recovery = cx.update(|window, cx| {
                    ade_workspaces::kill_and_recreate_workspace_sessions(
                        &workspace,
                        worktree_root,
                        ade_host,
                        window,
                        cx,
                    )
                })?;
                if let Err(error) = recovery.await {
                    let detail = format!("{error:#}");
                    let prompt = cx.update(|window, cx| {
                        window.prompt(
                            gpui::PromptLevel::Critical,
                            "Could not recreate persistent sessions",
                            Some(&detail),
                            &["OK"],
                            cx,
                        )
                    })?;
                    prompt.await.log_err();
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
    }

    /// Backs both the hover `…` button and right-clicking the row, so the two
    /// never drift apart.
    fn workspace_manager_row_menu(
        &self,
        context: WorkspaceRowContext,
        cx: &mut Context<Self>,
    ) -> impl Fn(&mut Window, &mut App) -> Entity<ContextMenu> + use<> {
        let sidebar = cx.weak_entity();
        let multi_workspace = self.multi_workspace.clone();
        let group_names: Vec<String> = self
            .workspace_groups
            .iter()
            .map(|group| group.name.clone())
            .collect();
        let is_pinned = context.worktree_root.as_ref().is_some_and(|root| {
            self.pinned_worktrees
                .iter()
                .any(|state| state.matches(root, context.host_key.as_deref()))
        });
        let is_unread = context.worktree_root.as_ref().is_some_and(|root| {
            self.unread_worktrees
                .iter()
                .any(|state| state.matches(root, context.host_key.as_deref()))
        });
        let is_grouped = context.project_key.as_ref().is_some_and(|key| {
            self.workspace_groups.iter().any(|group| {
                group
                    .projects
                    .iter()
                    .any(|state| state.matches(key, context.host_key.as_deref()))
            })
        });

        move |window, cx| {
            let context = context.clone();
            let sidebar = sidebar.clone();
            let multi_workspace = multi_workspace.clone();
            let group_names = group_names.clone();
            let host_key = context.host_key.clone();

            ContextMenu::build(window, cx, move |menu, _window, _cx| {
                use workspace_manager::RowKind;

                if let RowKind::Group(_) = context.kind {
                    let Some(group_index) = context.group_index else {
                        return menu;
                    };
                    let rename_sidebar = sidebar.clone();
                    let delete_sidebar = sidebar;
                    return menu
                        .entry("Rename Group", None, move |window, cx| {
                            rename_sidebar
                                .update(cx, |sidebar, cx| {
                                    sidebar.start_renaming_workspace_group(group_index, window, cx);
                                })
                                .ok();
                        })
                        .separator()
                        .entry("Delete Group", None, move |_window, cx| {
                            delete_sidebar
                                .update(cx, |sidebar, cx| {
                                    sidebar.delete_workspace_group(group_index, cx);
                                })
                                .ok();
                        });
                }

                let is_worktree = matches!(context.kind, RowKind::Worktree(_));
                let Some(key) = context.workspace_key.clone() else {
                    return menu;
                };
                let removal_keys = context.removal_keys.clone();

                let menu = menu.when(context.can_create_worktree, |menu| {
                    let key = key.clone();
                    let sidebar = sidebar.clone();
                    menu.entry("New Worktree", None, move |window, cx| {
                        sidebar
                            .update(cx, |sidebar, cx| {
                                if let Some(workspace) = sidebar.workspace_for_group(&key, cx) {
                                    sidebar.open_worktree_picker(&workspace, window, cx);
                                }
                            })
                            .ok();
                    })
                });

                let menu = menu.entry("Open in New Window", None, {
                    let key = key.clone();
                    let multi_workspace = multi_workspace.clone();
                    move |window, cx| {
                        multi_workspace
                            .update(cx, |multi_workspace, cx| {
                                multi_workspace
                                    .open_project_group_in_new_window(&key, window, cx)
                                    .detach_and_log_err(cx);
                            })
                            .ok();
                    }
                });

                let menu = menu.entry("Copy Path", None, {
                    let key = key.clone();
                    move |_window, cx| {
                        if let Some(path) = key.path_list().paths().first() {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                path.to_string_lossy().into_owned(),
                            ));
                        }
                    }
                });

                // One definition, shared with the center pane's "New…" menu.
                let agent_workspace = sidebar.upgrade().and_then(|sidebar| {
                    sidebar
                        .read(_cx)
                        .workspace_for_group(&key, _cx)
                        .map(|workspace| workspace.downgrade())
                });
                let menu = match agent_workspace {
                    Some(workspace) => {
                        agent_workspaces::append_terminal_agents(menu, workspace, _window, _cx)
                    }
                    None => menu,
                };

                let menu = menu.when_some(context.project_key.clone(), |menu, project_key| {
                    let host_key = host_key.clone();
                    let menu = menu.when(context.has_hidden_worktrees, |menu| {
                        let project_key = project_key.clone();
                        let sidebar = sidebar.clone();
                        let host_key = host_key.clone();
                        let label = if context.shows_hidden_worktrees {
                            "Hide hidden worktrees"
                        } else {
                            "Show hidden worktrees"
                        };
                        menu.entry(label, None, move |_window, cx| {
                            sidebar
                                .update(cx, |sidebar, cx| {
                                    sidebar.toggle_project_hidden_worktrees(
                                        project_key.clone(),
                                        host_key.clone(),
                                        cx,
                                    );
                                })
                                .ok();
                        })
                    });
                    let mut menu = menu.separator().entry("New Group from Project", None, {
                        let project_key = project_key.clone();
                        let sidebar = sidebar.clone();
                        let host_key = host_key.clone();
                        move |window, cx| {
                            let name = project_key
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "Group".to_owned());
                            sidebar
                                .update(cx, |sidebar, cx| {
                                    sidebar.create_workspace_group(
                                        &name,
                                        vec![workspace_manager::ScopedPath::new(
                                            project_key.clone(),
                                            host_key.clone(),
                                        )],
                                        window,
                                        cx,
                                    );
                                })
                                .ok();
                        }
                    });

                    for (group_index, group_name) in group_names.iter().enumerate() {
                        let project_key = project_key.clone();
                        let sidebar = sidebar.clone();
                        let host_key = host_key.clone();
                        menu = menu.entry(
                            format!("Move to {group_name}"),
                            None,
                            move |_window, cx| {
                                sidebar
                                    .update(cx, |sidebar, cx| {
                                        sidebar.move_project_to_workspace_group(
                                            workspace_manager::ScopedPath::new(
                                                project_key.clone(),
                                                host_key.clone(),
                                            ),
                                            Some(group_index),
                                            cx,
                                        );
                                    })
                                    .ok();
                            },
                        );
                    }

                    menu.when(is_grouped, |menu| {
                        let project_key = project_key.clone();
                        let sidebar = sidebar.clone();
                        let host_key = host_key.clone();
                        menu.entry("Remove from Group", None, move |_window, cx| {
                            sidebar
                                .update(cx, |sidebar, cx| {
                                    sidebar.move_project_to_workspace_group(
                                        workspace_manager::ScopedPath::new(
                                            project_key.clone(),
                                            host_key.clone(),
                                        ),
                                        None,
                                        cx,
                                    );
                                })
                                .ok();
                        })
                    })
                });

                let menu = menu.when_some(context.worktree_root.clone(), |menu, root| {
                    let sidebar = sidebar.clone();
                    let host_key = host_key.clone();
                    let label = if is_unread {
                        "Mark Read"
                    } else {
                        "Mark Unread"
                    };
                    menu.separator().entry(label, None, move |_window, cx| {
                        sidebar
                            .update(cx, |sidebar, cx| {
                                sidebar.toggle_worktree_unread(root.clone(), host_key.clone(), cx);
                            })
                            .ok();
                    })
                });

                let menu = menu.when_some(context.removable_worktree.clone(), |menu, root| {
                    let sidebar = sidebar.clone();
                    let name = context.worktree_name.clone().unwrap_or_default();
                    let host_key = context.host_key.clone();
                    let repository_key = context.worktree_project_key.clone();
                    menu.entry("Rename Worktree", None, move |window, cx| {
                        sidebar
                            .update(cx, |sidebar, cx| {
                                sidebar.start_renaming_worktree(
                                    root.clone(),
                                    host_key.clone(),
                                    repository_key.clone(),
                                    name.clone(),
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    })
                });

                let menu = menu.when_some(context.worktree_root.clone(), |menu, root| {
                    let sidebar = sidebar.clone();
                    let host_key = host_key.clone();
                    let label = if is_pinned { "Unpin" } else { "Pin" };
                    menu.entry(label, None, move |_window, cx| {
                        sidebar
                            .update(cx, |sidebar, cx| {
                                sidebar.toggle_worktree_pinned(root.clone(), host_key.clone(), cx);
                            })
                            .ok();
                    })
                });

                let menu = menu.when_some(
                    context
                        .can_hide_worktree
                        .then(|| {
                            context
                                .worktree_root
                                .clone()
                                .zip(context.worktree_project_key.clone())
                        })
                        .flatten(),
                    |menu, (root, project_key)| {
                        let sidebar = sidebar.clone();
                        let host_key = host_key.clone();
                        let label = if context.worktree_is_hidden {
                            "Unhide Worktree"
                        } else {
                            "Hide Worktree"
                        };
                        menu.entry(label, None, move |_window, cx| {
                            sidebar
                                .update(cx, |sidebar, cx| {
                                    sidebar.toggle_worktree_hidden(
                                        root.clone(),
                                        project_key.clone(),
                                        host_key.clone(),
                                        cx,
                                    );
                                })
                                .ok();
                        })
                    },
                );

                let menu = if is_worktree {
                    menu.when_some(context.worktree_workspace.clone(), |menu, workspace| {
                        let recovery_scope = context.worktree_root.clone().and_then(|root| {
                            workspace
                                .upgrade()
                                .is_some_and(|workspace| {
                                    ade_workspaces::can_reset_workspace_sessions(
                                        &workspace,
                                        &root,
                                        context.ade_host.as_deref(),
                                        _cx,
                                    )
                                })
                                .then(|| (root, context.ade_host.clone()))
                        });
                        let worktree_name = context.worktree_name.clone().unwrap_or_else(|| {
                            context
                                .worktree_root
                                .as_ref()
                                .and_then(|root| root.file_name())
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "worktree".to_owned())
                                .into()
                        });
                        let recovery_sidebar = sidebar.clone();
                        let recovery_workspace = workspace.clone();
                        let menu =
                            menu.separator()
                                .entry("Close Workspace", None, move |window, cx| {
                                    let Some(workspace) = workspace.upgrade() else {
                                        return;
                                    };
                                    multi_workspace
                                        .update(cx, |multi_workspace, cx| {
                                            multi_workspace
                                                .close_workspace(&workspace, window, cx)
                                                .detach_and_log_err(cx);
                                        })
                                        .ok();
                                });
                        menu.when_some(recovery_scope, |menu, (worktree_root, ade_host)| {
                            menu.entry("Kill and Recreate Sessions…", None, move |window, cx| {
                                recovery_sidebar
                                    .update(cx, |sidebar, cx| {
                                        sidebar.kill_and_recreate_workspace_sessions(
                                            recovery_workspace.clone(),
                                            worktree_name.clone(),
                                            worktree_root.clone(),
                                            ade_host.clone(),
                                            window,
                                            cx,
                                        );
                                    })
                                    .ok();
                            })
                        })
                    })
                } else {
                    menu.separator()
                        .entry("Remove Project", None, move |window, cx| {
                            multi_workspace
                                .update(cx, |multi_workspace, cx| {
                                    multi_workspace
                                        .remove_project_groups(&removal_keys, window, cx)
                                        .detach_and_log_err(cx);
                                })
                                .ok();
                        })
                };

                menu.when_some(context.removable_worktree.clone(), |menu, root| {
                    let sidebar = sidebar.clone();
                    let workspace = context.worktree_workspace.clone();
                    let group_key = context.workspace_key.clone();
                    let repository_key = context.worktree_project_key.clone();
                    menu.separator()
                        .entry("Delete Worktree", None, move |window, cx| {
                            sidebar
                                .update(cx, |sidebar, cx| {
                                    sidebar.delete_worktree(
                                        root.clone(),
                                        workspace.clone(),
                                        group_key.clone(),
                                        repository_key.clone(),
                                        window,
                                        cx,
                                    );
                                })
                                .ok();
                        })
                })
            })
        }
    }

    #[cfg(test)]
    fn toggle_collapse(
        &mut self,
        project_group_key: &ProjectGroupKey,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_collapsed = self.is_group_collapsed(project_group_key, cx);
        self.set_group_expanded(project_group_key, is_collapsed, cx);
        self.update_entries(cx);
    }

    /// Activates a workspace-manager row: opens the worktree it points at, or
    /// expands a group or project, which have no workspace of their own.
    ///
    /// Shared by clicking a row and confirming the keyboard cursor so the two
    /// cannot drift.
    fn activate_tree_row(
        &mut self,
        tree: &workspace_manager::WorkspaceTree,
        row_kind: workspace_manager::RowKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let toggle_key = Self::collapse_key_for_row(tree, &row_kind);
        let target = match row_kind {
            workspace_manager::RowKind::Worktree(id) => tree.workspace_for(id),
            _ => None,
        };
        let row_key = tree.group_key_for_row(&row_kind);
        let target_key = match row_kind {
            workspace_manager::RowKind::Worktree(id) => row_key
                .clone()
                .zip(tree.worktree_root(id))
                .map(|(key, root)| ProjectGroupKey::new(key.host(), PathList::new(&[root]))),
            _ => None,
        };
        let worktree_root = match row_kind {
            workspace_manager::RowKind::Worktree(id) => tree.worktree_root(id),
            _ => None,
        };

        // Activating a worktree is one of Orca's two ways of clearing its
        // unread dot.
        if let Some(root) = worktree_root.as_ref() {
            let host_key = row_key
                .as_ref()
                .and_then(ProjectGroupKey::host)
                .as_ref()
                .and_then(|host| workspace_manager::host_cache_key(Some(host)));
            self.clear_worktree_unread(root, host_key.as_deref(), cx);
        }

        match target_key.as_ref() {
            // One path for open, active, and closed worktrees alike: a
            // `WeakEntity` only upgrades while the workspace is open, so the
            // key is what reaches all three.
            Some(key) => match target.as_ref().and_then(|w| w.upgrade()) {
                // An open checkout is reached by its own entity; resolving it
                // by path would go back through the shared group key and land
                // on main again.
                Some(workspace) => {
                    self.activate_workspace(&workspace, window, cx);
                    if workspace.read(cx).active_pane().read(cx).items_len() == 0 {
                        self.create_new_terminal(&workspace, window, cx);
                    }
                }
                None => self.open_workspace_and_create_entry(
                    key,
                    NewEntryTarget::TerminalIfCentreEmpty,
                    window,
                    cx,
                ),
            },
            None => {
                if let Some(key) = toggle_key {
                    self.toggle_workspace_node_collapsed(key, cx);
                }
            }
        }
    }

    /// Number of rows the workspace manager is currently showing, which is
    /// what the keyboard cursor moves over.
    fn visible_row_count(&mut self, cx: &mut Context<Self>) -> usize {
        self.workspace_tree(cx).rows().len()
    }

    fn render_workspace_manager(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tree = self.workspace_tree(cx);

        let active_workspace = self
            .multi_workspace
            .upgrade()
            .map(|multi_workspace| multi_workspace.read(cx).workspace().clone());

        let rows = tree
            .rows()
            .into_iter()
            .enumerate()
            .map(|(ix, row)| {
                let toggle_key = Self::collapse_key_for_row(&tree, &row.kind);
                let row_kind = row.kind;

                // Clicking a worktree activates the workspace it belongs to;
                // clicking a group or project just expands it, matching Orca.
                let target = match row_kind {
                    workspace_manager::RowKind::Worktree(id) => tree.workspace_for(id),
                    _ => None,
                };
                // Reaches a worktree whether or not its workspace is open; a
                // `WeakEntity` only upgrades while it is.
                let row_key = tree.group_key_for_row(&row_kind);
                // Scoped to this checkout, not the repository: a
                // `ProjectGroupKey` carries the MAIN worktree's path and every
                // sibling shares it, so opening by it always landed on main.
                let _target_key = match row_kind {
                    workspace_manager::RowKind::Worktree(id) => {
                        row_key.zip(tree.worktree_root(id)).map(|(key, root)| {
                            ProjectGroupKey::new(key.host(), PathList::new(&[root]))
                        })
                    }
                    _ => None,
                };
                // The keyboard cursor wins while it is set; otherwise the row
                // for the active workspace stays highlighted.
                let is_selected = match self.selection {
                    Some(selected) => selected == ix,
                    None => target
                        .as_ref()
                        .zip(active_workspace.as_ref())
                        .is_some_and(|(target, active)| target.entity_id() == active.entity_id()),
                };

                let context = self.workspace_row_context(&tree, row_kind);
                let is_loading = self
                    .pending_worktree_open
                    .as_ref()
                    .is_some_and(|root| context.worktree_root.as_ref() == Some(root))
                    || context.worktree_root.as_ref().is_some_and(|root| {
                        self.pending_worktree_deletions
                            .iter()
                            .any(|path| path.matches(root, context.host_key.as_deref()))
                    });
                let hover_actions = self.render_workspace_manager_row_actions(ix, &context, cx);
                let daemon_upgrade_action = self.render_daemon_upgrade_action(&context, cx);

                let rename_editor = match row_kind {
                    workspace_manager::RowKind::Group(id) => tree
                        .group_name(id)
                        .and_then(|name| self.workspace_group_index(&name))
                        .filter(|index| self.renaming_workspace_group == Some(*index))
                        .map(|_| self.group_rename_editor.clone().into_any_element()),
                    workspace_manager::RowKind::Worktree(_) => context
                        .worktree_root
                        .as_ref()
                        .filter(|root| {
                            self.renaming_worktree.as_ref().is_some_and(
                                |(renaming_root, renaming_host_key, _)| {
                                    renaming_root == *root && renaming_host_key == &context.host_key
                                },
                            )
                        })
                        .map(|_| self.worktree_rename_editor.clone().into_any_element()),
                    _ => None,
                };

                let element = workspace_manager::render_row(
                    &tree,
                    &row,
                    ix,
                    is_selected,
                    is_loading,
                    cx.listener({
                        move |this, _, _window, cx| {
                            if let Some(key) = toggle_key.clone() {
                                this.toggle_workspace_node_collapsed(key, cx);
                            }
                        }
                    }),
                    cx.listener({
                        let tree = tree.clone();
                        move |this, _, window, cx| {
                            this.selection = Some(ix);
                            this.activate_tree_row(&tree, row_kind, window, cx);
                        }
                    }),
                    daemon_upgrade_action,
                    hover_actions,
                    rename_editor,
                );

                right_click_menu(SharedString::from(format!("workspace-manager-row-{ix}")))
                    .trigger(move |_, _, _| element)
                    .menu(self.workspace_manager_row_menu(context, cx))
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        v_flex()
            .flex_1()
            .min_h_0()
            // The traffic lights float over the sidebar, so the first row has to
            // clear them. The thread list gets this from its own header.
            .pt(ui::utils::platform_title_bar_height(window))
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .child(self.render_filter_input(cx)),
            )
            .child(workspace_manager::render_section_header(
                cx.listener(|this, _, window, cx| {
                    this.filter_editor.focus_handle(cx).focus(window, cx);
                }),
                cx.listener(|this, _, window, cx| {
                    this.create_workspace_group("Group", Vec::new(), window, cx);
                }),
                self.render_add_project_button(
                    "sidebar-header-add-project-menu",
                    "workspace-manager-add-project",
                    IconName::Plus,
                    gpui::Anchor::TopRight,
                    gpui::Point {
                        x: px(2.0),
                        y: px(2.0),
                    },
                    self.add_project_popover_handle.clone(),
                    true,
                    cx,
                )
                .into_any_element(),
            ))
            .child(v_flex().w_full().children(rows))
            .into_any_element()
    }
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _titlebar_height = ui::utils::platform_title_bar_height(window);
        let ui_font = theme_settings::setup_ui_font(window, cx);

        let color = cx.theme().colors();
        let bg = color
            .title_bar_background
            .blend(color.panel_background.opacity(0.25));

        v_flex()
            .id("workspace-sidebar")
            .key_context(self.dispatch_context(window, cx))
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::editor_move_down))
            .on_action(cx.listener(Self::editor_move_up))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::expand_selected_entry))
            .on_action(cx.listener(Self::collapse_selected_entry))
            .on_action(cx.listener(Self::toggle_selected_fold))
            .on_action(cx.listener(Self::fold_all))
            .on_action(cx.listener(Self::unfold_all))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::new_thread_in_group))
            .on_action(cx.listener(Self::archive_selected_thread))
            .on_action(cx.listener(Self::new_terminal_thread))
            .on_action(cx.listener(Self::focus_sidebar_filter))
            .on_action(cx.listener(Self::on_toggle_thread_switcher))
            .on_action(cx.listener(Self::on_next_project))
            .on_action(cx.listener(Self::on_previous_project))
            .on_action(cx.listener(|this, _: &OpenRecent, window, cx| {
                this.recent_projects_popover_handle.toggle(window, cx);
            }))
            .font(ui_font)
            .map(|el| {
                let on_left = self.side(cx) == SidebarSide::Left;
                match window.window_decorations() {
                    Decorations::Server => el.h_full().w(self.width),
                    // With client-side decorations the sidebar owns the window
                    // corners on its side, so round them like the title bar and
                    // status bar do. The sidebar is stretched 1px outwards over
                    // the window border on untiled edges (with compensating
                    // padding) so its rounded background lines up exactly with
                    // the window shape, avoiding a transparent gap in the
                    // rounded corners.
                    Decorations::Client { tiling, .. } => el
                        .absolute()
                        .top(if tiling.top { px(0.) } else { px(-1.) })
                        .bottom(if tiling.bottom { px(0.) } else { px(-1.) })
                        .when(!tiling.top, |el| el.pt_px())
                        .when(!tiling.bottom, |el| el.pb_px())
                        .map(|el| {
                            if on_left {
                                el.right(px(0.))
                                    .left(if tiling.left { px(0.) } else { px(-1.) })
                                    .when(!tiling.left, |el| el.pl(px(1.)))
                            } else {
                                el.left(px(0.))
                                    .right(if tiling.right { px(0.) } else { px(-1.) })
                                    .when(!tiling.right, |el| el.pr(px(1.)))
                            }
                        })
                        .when(on_left && !(tiling.top || tiling.left), |el| {
                            el.rounded_tl(CLIENT_SIDE_DECORATION_ROUNDING)
                        })
                        .when(on_left && !(tiling.bottom || tiling.left), |el| {
                            el.rounded_bl(CLIENT_SIDE_DECORATION_ROUNDING)
                        })
                        .when(!on_left && !(tiling.top || tiling.right), |el| {
                            el.rounded_tr(CLIENT_SIDE_DECORATION_ROUNDING)
                        })
                        .when(!on_left && !(tiling.bottom || tiling.right), |el| {
                            el.rounded_br(CLIENT_SIDE_DECORATION_ROUNDING)
                        }),
                }
            })
            .bg(bg)
            .when(self.side(cx) == SidebarSide::Left, |el| el.border_r_1())
            .when(self.side(cx) == SidebarSide::Right, |el| el.border_l_1())
            .border_color(color.border)
            .child(self.render_workspace_manager(window, cx))
            .child(self.render_sidebar_bottom_bar(cx))
            .into_any_element()
    }
}

pub fn dump_workspace_info(
    workspace: &mut Workspace,
    _: &DumpWorkspaceInfo,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    use std::fmt::Write;

    let mut output = String::new();
    let this_entity = cx.entity();

    let multi_workspace = workspace.multi_workspace().and_then(|weak| weak.upgrade());
    let workspaces: Vec<gpui::Entity<Workspace>> = match &multi_workspace {
        Some(mw) => mw.read(cx).workspaces().cloned().collect(),
        None => vec![this_entity.clone()],
    };
    let active_workspace = multi_workspace
        .as_ref()
        .map(|mw| mw.read(cx).workspace().clone());

    writeln!(output, "MultiWorkspace: {} workspace(s)", workspaces.len()).ok();

    if let Some(mw) = &multi_workspace {
        let keys: Vec<_> = mw.read(cx).project_group_keys();
        writeln!(output, "Project group keys ({}):", keys.len()).ok();
        for key in keys {
            writeln!(output, "  - {key:?}").ok();
        }
    }

    writeln!(output).ok();

    for (index, ws) in workspaces.iter().enumerate() {
        let is_active = active_workspace.as_ref() == Some(ws);
        writeln!(
            output,
            "--- Workspace {index}{} ---",
            if is_active { " (active)" } else { "" }
        )
        .ok();

        // project_group_key_for_workspace internally reads the workspace,
        // so we can only call it for workspaces other than this_entity
        // (which is already being updated).
        if let Some(mw) = &multi_workspace {
            if *ws == this_entity {
                let workspace_key = workspace.project_group_key(cx);
                writeln!(output, "ProjectGroupKey: {workspace_key:?}").ok();
            } else {
                let effective_key = mw.read(cx).project_group_key_for_workspace(ws, cx);
                let workspace_key = ws.read(cx).project_group_key(cx);
                if !effective_key.matches(&workspace_key) {
                    writeln!(
                        output,
                        "ProjectGroupKey (multi_workspace): {effective_key:?}"
                    )
                    .ok();
                    writeln!(
                        output,
                        "ProjectGroupKey (workspace, DISAGREES): {workspace_key:?}"
                    )
                    .ok();
                } else {
                    writeln!(output, "ProjectGroupKey: {effective_key:?}").ok();
                }
            }
        } else {
            let workspace_key = workspace.project_group_key(cx);
            writeln!(output, "ProjectGroupKey: {workspace_key:?}").ok();
        }

        // The action handler is already inside an update on `this_entity`,
        // so we must avoid a nested read/update on that same entity.
        if *ws == this_entity {
            dump_single_workspace(workspace, &mut output, cx);
        } else {
            ws.read_with(cx, |ws, cx| {
                dump_single_workspace(ws, &mut output, cx);
            });
        }
    }

    let project = workspace.project().clone();
    cx.spawn_in(window, async move |_this, cx| {
        let buffer = project
            .update(cx, |project, cx| project.create_buffer(None, false, cx))
            .await?;

        buffer.update(cx, |buffer, cx| {
            buffer.set_text(output, cx);
        });

        let buffer = cx.new(|cx| {
            editor::MultiBuffer::singleton(buffer, cx).with_title("Workspace Info".into())
        });

        _this.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(
                Box::new(cx.new(|cx| {
                    let mut editor =
                        editor::Editor::for_multibuffer(buffer, Some(project.clone()), window, cx);
                    editor.set_read_only(true);
                    editor.set_should_serialize(false, cx);
                    editor.set_breadcrumb_header("Workspace Info".into());
                    editor
                })),
                None,
                true,
                window,
                cx,
            );
        })
    })
    .detach_and_log_err(cx);
}

fn dump_single_workspace(workspace: &Workspace, output: &mut String, cx: &gpui::App) {
    use std::fmt::Write;

    let workspace_db_id = workspace.database_id();
    match workspace_db_id {
        Some(id) => writeln!(output, "Workspace DB ID: {id:?}").ok(),
        None => writeln!(output, "Workspace DB ID: (none)").ok(),
    };

    let project = workspace.project().read(cx);

    let projects: Vec<_> = project
        .repositories(cx)
        .values()
        .map(|project| project.read(cx).snapshot())
        .collect();

    writeln!(output, "Worktrees:").ok();
    for worktree in project.worktrees(cx) {
        let worktree = worktree.read(cx);
        let abs_path = worktree.abs_path();
        let visible = worktree.is_visible();

        let repo_info = projects
            .iter()
            .find(|snapshot| abs_path.starts_with(&*snapshot.work_directory_abs_path));

        let is_linked = repo_info.map(|s| s.is_linked_worktree()).unwrap_or(false);
        let main_worktree_path = repo_info.and_then(|s| s.main_worktree_abs_path());
        let branch = repo_info.and_then(|s| s.branch.as_ref().map(|b| b.ref_name.clone()));

        write!(output, "  - {}", abs_path.display()).ok();
        if !visible {
            write!(output, " (hidden)").ok();
        }
        if let Some(branch) = &branch {
            write!(output, " [branch: {branch}]").ok();
        }
        if is_linked {
            if let Some(main_worktree_path) = main_worktree_path {
                write!(
                    output,
                    " [linked worktree -> {}]",
                    main_worktree_path.display()
                )
                .ok();
            } else {
                write!(output, " [linked worktree]").ok();
            }
        }
        writeln!(output).ok();
    }

    writeln!(output).ok();
}
