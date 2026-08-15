//! Orca-shaped workspace manager: Group → Project → Worktree.
//!
//! Worktrees are leaves. Sessions deliberately do not appear here — Orca shows
//! them as tabs in the center pane, and ZOrca's agent panel already does the
//! same, so putting them in the tree would diverge from both.
//!
//! Rows are derived from the tree on every read rather than maintained
//! alongside it, which is what makes "every visible row is reachable by
//! expanding from a root" true by construction instead of by invariant.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use git::repository::Worktree as GitWorktree;
use gpui::{AnyElement, App, ClickEvent, Entity, SharedString, WeakEntity, px};
use project::{ProjectGroupKey, worktree_display_name};
use serde::{Deserialize, Serialize};
use ui::{Indicator, ListItem, SpinnerLabel, Tooltip, prelude::*};
use util::path_list::PathList;
use workspace::Workspace;

pub(crate) type RepositoryCacheKey = (PathBuf, Option<String>);
pub(crate) type AvailableWorktrees = HashMap<RepositoryCacheKey, Vec<GitWorktree>>;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum ScopedPath {
    Legacy(PathBuf),
    Scoped {
        path: PathBuf,
        host_key: Option<String>,
    },
}

impl ScopedPath {
    pub(crate) fn new(path: PathBuf, host_key: Option<String>) -> Self {
        Self::Scoped { path, host_key }
    }

    pub(crate) fn matches(&self, path: &Path, host_key: Option<&str>) -> bool {
        match self {
            Self::Legacy(_) => false,
            Self::Scoped {
                path: state_path,
                host_key: state_host_key,
            } => state_path == path && state_host_key.as_deref() == host_key,
        }
    }

    pub(crate) fn resolved(&self, scopes: &HashSet<(PathBuf, Option<String>)>) -> Option<Self> {
        match self {
            Self::Scoped { .. } => Some(self.clone()),
            Self::Legacy(path) => {
                let mut matches = scopes.iter().filter(|(candidate, _)| candidate == path);
                let Some((_, host_key)) = matches.next() else {
                    return Some(self.clone());
                };
                matches
                    .next()
                    .is_none()
                    .then(|| Self::new(path.clone(), host_key.clone()))
            }
        }
    }
}

#[cfg(test)]
impl From<PathBuf> for ScopedPath {
    fn from(path: PathBuf) -> Self {
        Self::new(path, None)
    }
}

#[cfg(test)]
impl From<&Path> for ScopedPath {
    fn from(path: &Path) -> Self {
        Self::new(path.to_path_buf(), None)
    }
}

pub(crate) fn host_cache_key(host: Option<&remote::RemoteConnectionOptions>) -> Option<String> {
    host.map(|host| remote::remote_connection_identity(host).persistence_key())
}

pub(crate) fn repository_cache_key(
    common_dir: &Path,
    host: Option<&remote::RemoteConnectionOptions>,
) -> RepositoryCacheKey {
    (common_dir.to_path_buf(), host_cache_key(host))
}

fn fallback_project_path<'a>(group_key: &'a ProjectGroupKey, folder_root: &'a Path) -> &'a Path {
    group_key
        .path_list()
        .paths()
        .iter()
        .find(|path| path.as_path() == folder_root)
        .or_else(|| {
            (group_key.path_list().paths().len() == 1)
                .then(|| group_key.path_list().paths().first())
                .flatten()
        })
        .map(PathBuf::as_path)
        .unwrap_or(folder_root)
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GroupId(pub usize);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProjectId(pub usize);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorktreeId(pub usize);

/// Orca gates this on a live PTY, and above that layers `working`,
/// `permission` and `done` from the agent's own reported state
/// (`src/renderer/src/lib/worktree-status.ts`, precedence: permission >
/// working > active > inactive). ZOrca has no agent-state signal to read yet,
/// so only the liveness gate is modelled — inventing the other three would
/// mean inventing their semantics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WorktreeStatus {
    /// A terminal is alive in this worktree.
    Active,
    /// Nothing is running here.
    #[default]
    Inactive,
}

#[derive(Clone, Debug)]
pub struct Worktree {
    pub id: WorktreeId,
    pub name: SharedString,
    /// Identifies this worktree's workspace even when it is closed, which a
    /// `WeakEntity` cannot — it only upgrades while the workspace is open.
    pub group_key: Option<ProjectGroupKey>,
    /// This checkout's own directory, which `group_key` cannot give: every
    /// linked worktree of a repository shares one `ProjectGroupKey`.
    pub folder_root: Option<PathBuf>,
    /// The project this worktree belongs to. Carried on the worktree so a
    /// pinned row, which is lifted out of its project, keeps the association.
    pub project_key: Arc<Path>,
    pub status: WorktreeStatus,
    /// Output arrived while the user was not engaged with this worktree.
    /// Orca keeps the dot until the worktree is activated or its pane touched.
    pub is_unread: bool,
    /// The repository's original clone directory. Orca stars this row and
    /// calls it the primary worktree; it is not a user-settable favourite.
    pub is_primary: bool,
    /// The workspace this worktree is open in, so clicking the row can
    /// activate it. `None` only in tests.
    pub workspace: Option<WeakEntity<Workspace>>,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub id: ProjectId,
    /// Identifies the project across rebuilds and restarts, so group
    /// membership can be persisted against it. The repository's common Git
    /// directory, or the project root when it is under no version control.
    pub key: Arc<Path>,
    pub name: SharedString,
    /// Whether the project is under Git at all. Without a repository there is
    /// nothing to create a worktree from, so the action must not be offered.
    pub has_repository: bool,
    /// Restore can merge stale linked-worktree identities into one project row.
    group_keys: Vec<ProjectGroupKey>,
    pub collapsed: bool,
    pub worktrees: Vec<Worktree>,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub id: GroupId,
    /// `None` until ZOrca has user-created groups: a project in no group
    /// renders at the top level rather than under a row that would just repeat
    /// the project's own name.
    pub name: Option<SharedString>,
    pub collapsed: bool,
    pub projects: Vec<Project>,
}

#[derive(Clone, Debug, Default)]
pub struct WorkspaceTree {
    /// Worktrees the user pinned. Orca lists these in a dedicated section
    /// above the tree and, by default, removes them from their project
    /// (`getPinnedWorktreeDisplayPolicy` returns `single-location` unless
    /// `showPinnedWorktreesInGroups` is set).
    pub pinned: Vec<Worktree>,
    pub pinned_collapsed: bool,
    pub groups: Vec<Group>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowKind {
    /// The header of the pinned section, which belongs to no group.
    PinnedSection,
    Group(GroupId),
    Project(ProjectId),
    Worktree(WorktreeId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    /// Indentation only. Never use this for identity — collapsing a parent
    /// changes a row's position but not which node it denotes.
    pub depth: usize,
    pub kind: RowKind,
    pub label: SharedString,
}

impl WorkspaceTree {
    /// Rows in display order, skipping the subtree of any collapsed node.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        if !self.pinned.is_empty() {
            rows.push(Row {
                depth: 0,
                kind: RowKind::PinnedSection,
                label: "Pinned".into(),
            });
            if !self.pinned_collapsed {
                for worktree in &self.pinned {
                    rows.push(Row {
                        depth: 1,
                        kind: RowKind::Worktree(worktree.id),
                        label: worktree.name.clone(),
                    });
                }
            }
        }
        for group in &self.groups {
            // A group with no name is the implicit "ungrouped" one: it
            // contributes no row, so its projects sit at the top level and
            // every descendant shifts up one.
            let depth = match &group.name {
                Some(name) => {
                    rows.push(Row {
                        depth: 0,
                        kind: RowKind::Group(group.id),
                        label: name.clone(),
                    });
                    if group.collapsed {
                        continue;
                    }
                    1
                }
                None => 0,
            };
            for project in &group.projects {
                rows.push(Row {
                    depth,
                    kind: RowKind::Project(project.id),
                    label: project.name.clone(),
                });
                if project.collapsed {
                    continue;
                }
                for worktree in &project.worktrees {
                    rows.push(Row {
                        depth: depth + 1,
                        kind: RowKind::Worktree(worktree.id),
                        label: worktree.name.clone(),
                    });
                }
            }
        }
        rows
    }

    /// The workspace a row's menu and `+` button act on. Groups and projects have
    /// no workspace of their own, so they borrow their first worktree's — which
    /// is the one Orca's project-level actions operate on too.
    pub fn group_key_for_row(&self, kind: &RowKind) -> Option<ProjectGroupKey> {
        let worktree = match kind {
            RowKind::PinnedSection => self.pinned.first(),
            RowKind::Group(id) => {
                self.groups
                    .iter()
                    .find(|group| group.id == *id)
                    .and_then(|group| {
                        group
                            .projects
                            .iter()
                            .flat_map(|project| project.worktrees.iter())
                            .next()
                    })
            }
            RowKind::Project(id) => self
                .groups
                .iter()
                .flat_map(|group| group.projects.iter())
                .find(|project| project.id == *id)
                .and_then(|project| project.worktrees.first()),
            RowKind::Worktree(id) => self.worktree(*id),
        };
        worktree.and_then(|worktree| worktree.group_key.clone())
    }

    /// Whether a project row may offer worktree creation.
    pub fn project_has_repository(&self, id: ProjectId) -> bool {
        self.groups
            .iter()
            .flat_map(|group| group.projects.iter())
            .find(|project| project.id == id)
            .is_some_and(|project| project.has_repository)
    }

    pub fn project_group_keys(&self, id: ProjectId) -> &[ProjectGroupKey] {
        self.groups
            .iter()
            .flat_map(|group| group.projects.iter())
            .find(|project| project.id == id)
            .map(|project| project.group_keys.as_slice())
            .unwrap_or_default()
    }

    pub fn project_key(&self, id: ProjectId) -> Option<Arc<Path>> {
        self.groups
            .iter()
            .flat_map(|group| group.projects.iter())
            .find(|project| project.id == id)
            .map(|project| project.key.clone())
    }

    pub fn worktree_is_primary(&self, id: WorktreeId) -> bool {
        self.worktree(id)
            .is_some_and(|worktree| worktree.is_primary)
    }

    pub fn worktree_project_key(&self, id: WorktreeId) -> Option<PathBuf> {
        self.worktree(id)
            .map(|worktree| worktree.project_key.to_path_buf())
    }

    pub fn worktree_name(&self, id: WorktreeId) -> Option<SharedString> {
        self.worktree(id).map(|worktree| worktree.name.clone())
    }

    pub fn group_name(&self, id: GroupId) -> Option<SharedString> {
        self.groups
            .iter()
            .find(|group| group.id == id)
            .and_then(|group| group.name.clone())
    }

    /// A worktree's root, whether or not Git would let us remove it.
    pub fn worktree_root(&self, id: WorktreeId) -> Option<PathBuf> {
        self.worktree(id)?.folder_root.clone()
    }

    pub fn removable_worktree_root(&self, id: WorktreeId) -> Option<std::path::PathBuf> {
        let project = self
            .groups
            .iter()
            .flat_map(|group| &group.projects)
            .find(|project| project.worktrees.iter().any(|worktree| worktree.id == id))?;
        let worktree = project
            .worktrees
            .iter()
            .find(|worktree| worktree.id == id)?;
        let root = worktree.folder_root.clone()?;
        (project.has_repository && !worktree.is_primary).then_some(root)
    }

    pub fn workspace_for(&self, id: WorktreeId) -> Option<WeakEntity<Workspace>> {
        self.worktree(id)
            .and_then(|worktree| worktree.workspace.clone())
    }

    fn worktree(&self, id: WorktreeId) -> Option<&Worktree> {
        self.pinned
            .iter()
            .chain(
                self.groups
                    .iter()
                    .flat_map(|group| group.projects.iter())
                    .flat_map(|project| project.worktrees.iter()),
            )
            .find(|worktree| worktree.id == id)
    }

    /// Whether the node a row denotes is collapsed. `None` for worktrees,
    /// which are leaves, and that is what suppresses their disclosure triangle.
    fn is_collapsed(&self, kind: &RowKind) -> Option<bool> {
        match kind {
            RowKind::PinnedSection => Some(self.pinned_collapsed),
            RowKind::Group(id) => self
                .groups
                .iter()
                .find(|group| group.id == *id)
                .map(|group| group.collapsed),
            RowKind::Project(id) => self
                .groups
                .iter()
                .flat_map(|group| group.projects.iter())
                .find(|project| project.id == *id)
                .map(|project| project.collapsed),
            RowKind::Worktree(_) => None,
        }
    }

    /// Toggles the collapsed state of whichever node the row denotes.
    /// Worktrees are leaves, so they are a no-op.
    pub fn toggle_collapsed(&mut self, kind: &RowKind) {
        match kind {
            RowKind::PinnedSection => self.pinned_collapsed = !self.pinned_collapsed,
            RowKind::Group(id) => {
                if let Some(group) = self.groups.iter_mut().find(|group| group.id == *id) {
                    group.collapsed = !group.collapsed;
                }
            }
            RowKind::Project(id) => {
                if let Some(project) = self
                    .groups
                    .iter_mut()
                    .flat_map(|group| group.projects.iter_mut())
                    .find(|project| project.id == *id)
                {
                    project.collapsed = !project.collapsed;
                }
            }
            RowKind::Worktree(_) => {}
        }
    }
}

/// Builds the tree from the open workspaces. ZOrca models a worktree as its
/// own `Workspace`, so workspaces sharing a `ProjectGroupKey` become sibling
/// worktrees under the repository they belong to.
pub fn build_tree(
    workspaces: &[Entity<Workspace>],
    available_worktrees: &AvailableWorktrees,
    closed_groups: &[ProjectGroupKey],
    cx: &App,
) -> WorkspaceTree {
    struct WorktreeRow {
        name: SharedString,
        group_key: ProjectGroupKey,
        /// This checkout's own directory. `ProjectGroupKey` carries the MAIN
        /// worktree's path, which every linked worktree of a repository shares,
        /// so it cannot place or distinguish a checkout.
        folder_root: Option<PathBuf>,
        is_primary: bool,
        workspace: Option<WeakEntity<Workspace>>,
        status: WorktreeStatus,
    }
    // Keyed by the repository's common Git directory. Every linked worktree of
    // a repository shares it, which is what makes them siblings under one
    // project instead of each becoming a project of its own — the working
    // directory name cannot do this, since worktrees are often named alike.
    let mut projects: BTreeMap<
        (Arc<Path>, Option<String>),
        (SharedString, bool, Vec<WorktreeRow>, Vec<ProjectGroupKey>),
    > = BTreeMap::new();

    for workspace_handle in workspaces {
        let workspace = workspace_handle.read(cx);
        let project = workspace.project().read(cx);

        // A worktree is identified by its own root, not by its branch: a
        // worktree created from the current HEAD is detached and has no branch,
        // and two detached worktrees would otherwise collapse into one row.
        let worktree_key = workspace.project_group_key(cx);
        let host_key = host_cache_key(worktree_key.host().as_ref());

        // Orca's liveness gate is a live PTY for the worktree; a terminal open
        // in one of the workspace's panes is ZOrca's equivalent.
        let has_terminal = workspace.panes().iter().any(|pane| {
            pane.read(cx)
                .items_of_type::<terminal_view::TerminalView>()
                .next()
                .is_some()
        });
        let status = if has_terminal {
            WorktreeStatus::Active
        } else {
            WorktreeStatus::Inactive
        };

        let roots = project
            .visible_worktrees(cx)
            .map(|worktree| {
                let worktree = worktree.read(cx);
                (
                    worktree.abs_path().to_path_buf(),
                    worktree.root_repo_common_dir().cloned(),
                    worktree.root_repo_is_linked_worktree(),
                )
            })
            .collect::<Vec<_>>();

        for (folder_root, root_repository_key, root_is_linked_worktree) in roots {
            let directory_name = folder_root
                .file_name()
                .map(|name| SharedString::from(name.to_string_lossy().into_owned()));
            let root: Arc<Path> = folder_root.as_path().into();

            // A project folder can contain many repositories — those are its
            // contents, not sibling projects. The one that identifies this
            // workspace is the innermost repository its root lives in; without
            // this, opening a folder of checkouts scattered a worktree row of it
            // under every repository beneath it.
            let owning_repository = project
                .repositories(cx)
                .values()
                .map(|repository| repository.read(cx))
                .filter(|repository| root.starts_with(&repository.work_directory_abs_path))
                .max_by_key(|repository| repository.work_directory_abs_path.components().count());

            let (key, name, worktree_display_name, has_repository) =
                if let Some(repository) = owning_repository {
                    let key = repository.common_dir_abs_path.clone();
                    let name = project_name(&key);
                    let worktree_display_name =
                        worktree_name(repository.main_worktree_abs_path(), Some(&folder_root))
                            .or_else(|| directory_name.clone())
                            .or_else(|| {
                                repository
                                    .branch
                                    .as_ref()
                                    .map(|branch| SharedString::from(branch.name().to_owned()))
                            })
                            .unwrap_or_else(|| SharedString::from("worktree"));
                    (key, name, worktree_display_name, true)
                } else if let Some(key) = root_repository_key {
                    let name = project_name(&key);
                    let worktree_display_name =
                        worktree_name(main_worktree_path(&key), Some(&folder_root))
                            .or_else(|| directory_name.clone())
                            .unwrap_or_else(|| SharedString::from("worktree"));
                    (key, name, worktree_display_name, true)
                } else {
                    // Restored linked workspaces carry their main repository path
                    // before Git discovery has populated the repository model.
                    let project_path = fallback_project_path(&worktree_key, &root);
                    let name = project_path
                        .file_name()
                        .map(|name| SharedString::from(name.to_string_lossy().into_owned()))
                        .unwrap_or_else(|| SharedString::from("Project"));
                    let worktree_display_name = worktree_name(Some(project_path), Some(&root))
                        .or_else(|| directory_name.clone())
                        .unwrap_or_else(|| SharedString::from("main"));
                    (project_path.into(), name, worktree_display_name, false)
                };

            let (_, _, worktree_rows, group_keys) = projects
                .entry((key, host_key.clone()))
                .or_insert_with(|| (name, has_repository, Vec::new(), Vec::new()));
            if !group_keys.iter().any(|key| key.matches(&worktree_key)) {
                group_keys.push(worktree_key.clone());
            }
            if let Some(existing) = worktree_rows.iter_mut().find(|row| {
                row.group_key.matches(&worktree_key)
                    && row.folder_root.as_ref() == Some(&folder_root)
            }) {
                if status == WorktreeStatus::Active {
                    existing.status = status;
                    existing.workspace = Some(workspace_handle.downgrade());
                }
                existing.is_primary |= has_repository && !root_is_linked_worktree;
            } else {
                worktree_rows.push(WorktreeRow {
                    name: worktree_display_name,
                    group_key: worktree_key.clone(),
                    folder_root: Some(folder_root),
                    is_primary: has_repository && !root_is_linked_worktree,
                    workspace: Some(workspace_handle.downgrade()),
                    status,
                });
            }
        }
    }

    for ((project_key, available_host), available) in available_worktrees {
        let main_worktree_path = main_worktree_path(project_key);
        for ((known_project_key, known_host), (_, _, worktree_rows, _)) in projects.iter_mut() {
            if known_project_key.as_ref() != project_key.as_path() || known_host != available_host {
                continue;
            }
            let Some(host) = worktree_rows
                .first()
                .map(|worktree| worktree.group_key.host())
            else {
                continue;
            };
            for worktree in available.iter().filter(|worktree| !worktree.is_bare) {
                let folder_root = worktree.path.clone();
                let name = worktree_name(main_worktree_path, Some(&folder_root))
                    .or_else(|| {
                        worktree
                            .ref_name
                            .as_deref()
                            .map(|name| name.strip_prefix("refs/heads/").unwrap_or(name).into())
                    })
                    .unwrap_or_else(|| SharedString::from("worktree"));
                if worktree.is_main
                    && let Some(row) = worktree_rows.iter_mut().find(|row| row.is_primary)
                {
                    row.name = name;
                    continue;
                }
                if let Some(row) = worktree_rows
                    .iter_mut()
                    .find(|row| row.folder_root.as_ref() == Some(&folder_root))
                {
                    row.name = name;
                    continue;
                }
                worktree_rows.push(WorktreeRow {
                    name,
                    group_key: ProjectGroupKey::new(
                        host.clone(),
                        PathList::new(std::slice::from_ref(&folder_root)),
                    ),
                    folder_root: Some(folder_root),
                    is_primary: worktree.is_main,
                    workspace: None,
                    status: WorktreeStatus::Inactive,
                });
            }
        }
    }

    // Projects the window knows but has not reopened. A restored window brings
    // back only its active workspace; the rest of its projects survive as
    // group keys alone, and a bar that lists only open workspaces would read
    // as having lost them. Their row opens the group the same way the recent
    // list does, so "closed" here still means one click away.
    for group_key in closed_groups {
        let Some(folder_root) = group_key.path_list().paths().first().cloned() else {
            continue;
        };
        let covered = projects
            .values()
            .any(|(_, _, _, keys)| keys.iter().any(|key| key.matches(group_key)));
        if covered {
            continue;
        }

        let group_host_key = host_cache_key(group_key.host().as_ref());
        if let Some(((common_dir, _), _)) =
            available_worktrees
                .iter()
                .find(|((common_dir, host), worktrees)| {
                    *host == group_host_key
                        && (worktrees
                            .iter()
                            .any(|worktree| worktree.path == folder_root)
                            || group_key
                                .path_list()
                                .paths()
                                .iter()
                                .any(|path| path == project::repo_identity_path(common_dir)))
                })
        {
            let main_root = project::repo_identity_path(common_dir).to_path_buf();
            let normalized_key = ProjectGroupKey::new(
                group_key.host(),
                PathList::new(std::slice::from_ref(&main_root)),
            );
            let name = project_name(common_dir);
            let worktree_name = worktree_name(Some(&main_root), Some(&folder_root))
                .unwrap_or_else(|| SharedString::from("worktree"));
            let (_, _, rows, group_keys) = projects
                .entry((common_dir.as_path().into(), group_host_key))
                .or_insert_with(|| (name, true, Vec::new(), Vec::new()));
            group_keys.push(group_key.clone());
            if !rows
                .iter()
                .any(|row| row.folder_root.as_ref() == Some(&folder_root))
            {
                let is_primary = is_main_worktree(common_dir, &folder_root);
                rows.push(WorktreeRow {
                    name: worktree_name,
                    group_key: normalized_key,
                    folder_root: Some(folder_root),
                    is_primary,
                    workspace: None,
                    status: WorktreeStatus::Inactive,
                });
            }
            continue;
        }

        let host_key = host_cache_key(group_key.host().as_ref());
        if let Some((_, _, _, group_keys)) =
            projects.get_mut(&(folder_root.as_path().into(), host_key.clone()))
        {
            group_keys.push(group_key.clone());
            continue;
        }
        let name = folder_root
            .file_name()
            .map(|name| SharedString::from(name.to_string_lossy().into_owned()))
            .unwrap_or_else(|| SharedString::from("Project"));
        projects.insert(
            (folder_root.as_path().into(), host_key),
            (
                name.clone(),
                // Unknowable while closed, and `false` is the honest reading:
                // no worktree can be created from a repository nobody can see.
                false,
                vec![WorktreeRow {
                    name: "main".into(),
                    group_key: group_key.clone(),
                    folder_root: Some(folder_root),
                    is_primary: false,
                    workspace: None,
                    status: WorktreeStatus::Inactive,
                }],
                vec![group_key.clone()],
            ),
        );
    }

    if projects.is_empty() {
        return WorkspaceTree::default();
    }

    let mut next_id = 0;
    let mut id = || {
        next_id += 1;
        next_id - 1
    };

    let projects = projects
        .into_iter()
        .map(
            |((key, _), (project_name, has_repository, mut worktrees, group_keys))| {
                worktrees.sort_by(|left, right| {
                    let rank = |row: &WorktreeRow| {
                        worktree_sort_key(row.is_primary, &row.name, row.folder_root.as_ref())
                    };
                    rank(left).cmp(&rank(right))
                });
                let project_key = key.clone();
                Project {
                    id: ProjectId(id()),
                    key,
                    name: project_name,
                    has_repository,
                    group_keys,
                    collapsed: false,
                    worktrees: worktrees
                        .into_iter()
                        .map(|row| Worktree {
                            id: WorktreeId(id()),
                            name: row.name,
                            group_key: Some(row.group_key),
                            folder_root: row.folder_root,
                            project_key: project_key.clone(),
                            status: row.status,
                            is_unread: false,
                            is_primary: row.is_primary,
                            workspace: row.workspace,
                        })
                        .collect(),
                }
            },
        )
        .collect();

    WorkspaceTree {
        pinned: Vec::new(),
        pinned_collapsed: false,
        groups: vec![Group {
            id: GroupId(id()),
            name: None,
            collapsed: false,
            projects,
        }],
    }
}

pub(crate) fn apply_hidden_worktrees(
    tree: &mut WorkspaceTree,
    hidden_worktrees: &HashSet<ScopedPath>,
    projects_showing_hidden_worktrees: &HashSet<ScopedPath>,
) {
    let is_visible = |worktree: &Worktree| {
        let host_key = worktree
            .group_key
            .as_ref()
            .and_then(ProjectGroupKey::host)
            .as_ref()
            .and_then(|host| host_cache_key(Some(host)));
        !worktree.folder_root.as_ref().is_some_and(|root| {
            hidden_worktrees
                .iter()
                .any(|state| state.matches(root, host_key.as_deref()))
        }) || projects_showing_hidden_worktrees
            .iter()
            .any(|state| state.matches(worktree.project_key.as_ref(), host_key.as_deref()))
    };
    tree.pinned.retain(&is_visible);
    for project in tree
        .groups
        .iter_mut()
        .flat_map(|group| group.projects.iter_mut())
    {
        project.worktrees.retain(&is_visible);
    }
}

/// Ordering within a project. Orca hoists a repository's main worktree to the
/// top ("hoists the repo main worktree first, matching the rendered sidebar");
/// the rest follow by display name, with the root as a tie-breaker so two
/// detached worktrees sharing a name stay distinct.
fn worktree_sort_key(
    is_primary: bool,
    name: &SharedString,
    root: Option<&PathBuf>,
) -> (bool, SharedString, Option<PathBuf>) {
    (!is_primary, name.clone(), root.cloned())
}

fn worktree_name(
    main_worktree_path: Option<&Path>,
    folder_root: Option<&Path>,
) -> Option<SharedString> {
    let root = folder_root?;
    if let Some(main_worktree_path) = main_worktree_path {
        return Some(worktree_display_name(main_worktree_path, root));
    }
    root.file_name()
        .map(|name| SharedString::from(name.to_string_lossy().into_owned()))
}

fn main_worktree_path(project_key: &Path) -> Option<&Path> {
    project_key
        .file_name()
        .is_some_and(|name| name == ".git")
        .then(|| project_key.parent())
        .flatten()
}

/// Whether `worktree_root` is the repository's main checkout, which Git refuses
/// to remove. The common Git directory of a normal checkout is `<main>/.git`,
/// so its parent is the main worktree; a bare repository has no main checkout
/// and every worktree under it is removable.
pub fn is_main_worktree(project_key: &Path, worktree_root: &Path) -> bool {
    project_key.file_name().is_some_and(|name| name == ".git")
        && project_key.parent() == Some(worktree_root)
}

/// Narrows the tree to what matches `query`, case-insensitively.
///
/// A node matching by its own name keeps its whole subtree, so filtering to a
/// project still shows every worktree in it. A node kept only because a
/// descendant matched shows just the matching descendants.
pub fn filter_tree(tree: &mut WorkspaceTree, query: &str) {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return;
    }
    let matches = |name: &SharedString| name.to_lowercase().contains(&query);

    tree.groups.retain_mut(|group| {
        if group.name.as_ref().is_some_and(matches) {
            return true;
        }
        group.projects.retain_mut(|project| {
            if matches(&project.name) {
                return true;
            }
            project.worktrees.retain(|worktree| matches(&worktree.name));
            !project.worktrees.is_empty()
        });
        !group.projects.is_empty()
    });
}

/// Marks the worktrees the user has not caught up with.
pub(crate) fn apply_unread(tree: &mut WorkspaceTree, unread_roots: &[ScopedPath]) {
    if unread_roots.is_empty() {
        return;
    }
    let mark = |worktree: &mut Worktree| {
        let host_key = worktree
            .group_key
            .as_ref()
            .and_then(ProjectGroupKey::host)
            .as_ref()
            .and_then(|host| host_cache_key(Some(host)));
        worktree.is_unread = worktree.folder_root.as_ref().is_some_and(|root| {
            unread_roots
                .iter()
                .any(|state| state.matches(root, host_key.as_deref()))
        });
    };
    tree.pinned.iter_mut().for_each(mark);
    for group in &mut tree.groups {
        for project in &mut group.projects {
            project.worktrees.iter_mut().for_each(mark);
        }
    }
}

/// Lifts the pinned worktrees into their own section above the tree.
///
/// Orca's default display policy is `single-location`: a pinned worktree
/// leaves its project rather than appearing twice. Pinning nothing leaves the
/// tree exactly as it was.
pub(crate) fn apply_pins(tree: &mut WorkspaceTree, pinned_roots: &[ScopedPath]) {
    if pinned_roots.is_empty() {
        return;
    }
    // Matched on the checkout's own directory: a `ProjectGroupKey` is shared by
    // every worktree of a repository, so pinning one would pin them all.
    let is_pinned = |worktree: &Worktree| {
        let host_key = worktree
            .group_key
            .as_ref()
            .and_then(ProjectGroupKey::host)
            .as_ref()
            .and_then(|host| host_cache_key(Some(host)));
        worktree.folder_root.as_ref().is_some_and(|root| {
            pinned_roots
                .iter()
                .any(|state| state.matches(root, host_key.as_deref()))
        })
    };

    let mut pinned = Vec::new();
    for group in &mut tree.groups {
        for project in &mut group.projects {
            project.worktrees.retain(|worktree| {
                if is_pinned(worktree) {
                    pinned.push(worktree.clone());
                    return false;
                }
                true
            });
        }
        // A project emptied by pinning would otherwise render as a bare row
        // with nothing under it.
        group
            .projects
            .retain(|project| !project.worktrees.is_empty());
    }
    tree.groups.retain(|group| !group.projects.is_empty());
    tree.pinned = pinned;
}

/// Moves each project into the first group that claims its key. Projects no
/// group claims stay in the unnamed group, which renders no row of its own, so
/// grouping nothing leaves the tree exactly as it was.
///
/// Named groups come first: they are the arrangement the user built, and
/// leaving ungrouped projects above them would bury it.
pub(crate) fn apply_groups(tree: &mut WorkspaceTree, groups: &[(SharedString, Vec<ScopedPath>)]) {
    let mut ungrouped: Vec<Project> = tree
        .groups
        .drain(..)
        .flat_map(|group| group.projects)
        .collect();

    let mut next_id = ungrouped
        .iter()
        .map(|project| project.id.0)
        .max()
        .map_or(0, |id| id + 1);
    let mut id = || {
        next_id += 1;
        next_id - 1
    };

    let mut grouped = Vec::new();
    for (name, keys) in groups {
        let mut claimed = Vec::new();
        // Retain what this group does not claim, so a project cannot land in
        // two groups even if the stored membership lists it twice.
        ungrouped.retain(|project| {
            let host_key = project
                .worktrees
                .first()
                .and_then(|worktree| worktree.group_key.as_ref())
                .and_then(ProjectGroupKey::host)
                .as_ref()
                .and_then(|host| host_cache_key(Some(host)));
            let is_claimed = keys
                .iter()
                .any(|key| key.matches(project.key.as_ref(), host_key.as_deref()));
            if is_claimed {
                claimed.push(project.clone());
            }
            !is_claimed
        });
        grouped.push(Group {
            id: GroupId(id()),
            name: Some(name.clone()),
            collapsed: false,
            projects: claimed,
        });
    }

    if !ungrouped.is_empty() {
        grouped.push(Group {
            id: GroupId(id()),
            name: None,
            collapsed: false,
            projects: ungrouped,
        });
    }

    tree.groups = grouped;
}

/// The project's name as Orca shows it: the main worktree's directory. The
/// common Git directory is `<main>/.git` for a normal checkout, and for a bare
/// repository there is no main worktree to name it after, so the directory
/// itself is used.
fn project_name(common_dir: &Path) -> SharedString {
    if common_dir.file_name().is_some_and(|name| name == ".git")
        && let Some(name) = common_dir.parent().and_then(|path| path.file_name())
    {
        return name.to_string_lossy().into_owned().into();
    }
    common_dir
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Project".to_owned())
        .into()
}

impl WorktreeStatus {
    fn indicator_color(self) -> Color {
        match self {
            Self::Inactive => Color::Muted,
            Self::Active => Color::Success,
        }
    }
}

/// The "Projects" bar above the tree, carrying Orca's filter, new-group and
/// add-project actions.
pub fn render_section_header(
    on_filter: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_new_group: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    add_project: AnyElement,
) -> AnyElement {
    h_flex()
        .w_full()
        .px_2()
        .py_1()
        .justify_between()
        .child(
            Label::new("Projects")
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .child(
            h_flex()
                .gap_1()
                .child(
                    IconButton::new("workspace-manager-filter", IconName::Filter)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Filter Projects"))
                        .on_click(on_filter),
                )
                .child(
                    // Deliberately not a folder-with-plus: that icon is the
                    // "Add Project" button, and a group holds projects rather
                    // than being one.
                    IconButton::new("workspace-manager-new-group", IconName::SquarePlus)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("New Group"))
                        .on_click(on_new_group),
                )
                .child(add_project),
        )
        .into_any_element()
}

fn remote_project_info(tree: &WorkspaceTree, kind: &RowKind) -> Option<SharedString> {
    let RowKind::Project(id) = kind else {
        return None;
    };
    let remote = tree.group_key_for_row(kind)?.host()?;
    let project = tree
        .groups
        .iter()
        .flat_map(|group| &group.projects)
        .find(|project| project.id == *id)?;
    let root = project
        .worktrees
        .iter()
        .find(|worktree| worktree.is_primary)
        .or_else(|| project.worktrees.first())?
        .folder_root
        .as_deref()?;
    let destination =
        ade_workspaces::destination_for(&remote).unwrap_or_else(|| remote.display_name());
    Some(format!("{destination} · {}", root.display()).into())
}

/// Renders one row. `on_toggle` fires for the disclosure triangle, `on_click`
/// for the row body, so expanding a project never also activates it.
pub fn render_row(
    tree: &WorkspaceTree,
    row: &Row,
    ix: usize,
    is_selected: bool,
    is_loading: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    end_slot: Option<AnyElement>,
    hover_actions: Option<AnyElement>,
    // `label` replaces the row's own label, for a row being renamed in place.
    label: Option<AnyElement>,
) -> AnyElement {
    let collapsed = tree.is_collapsed(&row.kind);
    let project_info = remote_project_info(tree, &row.kind);
    let project_info_tooltip = project_info.clone();

    let start_slot = match &row.kind {
        RowKind::PinnedSection => Icon::new(IconName::Pin)
            .size(IconSize::Small)
            .color(Color::Muted)
            .into_any_element(),
        RowKind::Group(_) => Icon::new(IconName::Folder)
            .size(IconSize::Small)
            .color(Color::Muted)
            .into_any_element(),
        // Not a folder icon: a project row reading as a folder is what made
        // these look like groups.
        RowKind::Project(_) => Icon::new(IconName::GitGraph)
            .size(IconSize::Small)
            .color(Color::Muted)
            .into_any_element(),
        RowKind::Worktree(_) if is_loading => SpinnerLabel::new()
            .size(LabelSize::Small)
            .into_any_element(),
        RowKind::Worktree(id) => {
            let worktree = tree.worktree(*id);
            let color = match worktree {
                // Unread outranks a passive status, matching Orca's status
                // slot, but never hides a worktree that is actually running.
                Some(worktree)
                    if worktree.is_unread && worktree.status == WorktreeStatus::Inactive =>
                {
                    Color::Info
                }
                Some(worktree) => worktree.status.indicator_color(),
                None => WorktreeStatus::default().indicator_color(),
            };
            Indicator::dot().color(color).into_any_element()
        }
    };

    let is_primary = match &row.kind {
        RowKind::Worktree(id) => tree
            .worktree(*id)
            .is_some_and(|worktree| worktree.is_primary),
        _ => false,
    };

    ListItem::new(ix)
        .indent_level(row.depth)
        .indent_step_size(px(12.))
        .selectable(true)
        .toggle_state(is_selected)
        .when_some(collapsed, |this, collapsed| {
            this.toggle(!collapsed).on_toggle(on_toggle)
        })
        .start_slot(start_slot)
        .child(label.unwrap_or_else(|| {
            h_flex()
                .w_full()
                .min_w_0()
                .gap_1p5()
                .child(Label::new(row.label.clone()).single_line())
                .when_some(project_info, |this, info| {
                    this.child(
                        Label::new(info)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    )
                })
                .into_any_element()
        }))
        .when_some(project_info_tooltip, |this, info| {
            this.tooltip(Tooltip::text(info))
        })
        .when(is_primary, |this| {
            this.end_slot(
                IconButton::new("workspace-manager-primary-worktree", IconName::StarFilled)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Warning)
                    .aria_label("Primary worktree (original clone directory)"),
            )
            .tooltip(Tooltip::text("Primary worktree (original clone directory)"))
        })
        .when_some(end_slot, ListItem::end_slot)
        .when_some(hover_actions, ListItem::end_slot_on_hover)
        .on_click(on_click)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_key(id: u64, path: &str) -> ProjectGroupKey {
        ProjectGroupKey::new(
            Some(remote::RemoteConnectionOptions::Mock(
                remote::MockConnectionOptions { id },
            )),
            PathList::new(&[PathBuf::from(path)]),
        )
    }

    fn worktree(id: usize, name: &str) -> Worktree {
        Worktree {
            id: WorktreeId(id),
            name: name.into(),
            group_key: None,
            folder_root: None,
            project_key: Path::new("/src/first-project/.git").into(),
            status: WorktreeStatus::Inactive,
            is_unread: false,
            is_primary: false,
            workspace: None,
        }
    }

    /// One group holding two projects, each with a `main` worktree — the
    /// smallest shape that exercises all three levels.
    fn tree() -> WorkspaceTree {
        WorkspaceTree {
            pinned: Vec::new(),
            pinned_collapsed: false,
            groups: vec![Group {
                id: GroupId(0),
                name: Some("group".into()),
                collapsed: false,
                projects: vec![
                    Project {
                        id: ProjectId(0),
                        key: Path::new("/src/first-project/.git").into(),
                        name: "first-project".into(),
                        has_repository: true,
                        group_keys: Vec::new(),
                        collapsed: false,
                        worktrees: vec![worktree(0, "main")],
                    },
                    Project {
                        id: ProjectId(1),
                        key: Path::new("/src/second-project/.git").into(),
                        name: "second-project".into(),
                        has_repository: true,
                        group_keys: Vec::new(),
                        collapsed: false,
                        worktrees: vec![worktree(1, "main")],
                    },
                ],
            }],
        }
    }

    fn shape(tree: &WorkspaceTree) -> Vec<(usize, RowKind)> {
        tree.rows()
            .into_iter()
            .map(|row| (row.depth, row.kind))
            .collect()
    }

    #[test]
    fn test_rows_nest_group_repo_worktree() {
        assert_eq!(
            shape(&tree()),
            vec![
                (0, RowKind::Group(GroupId(0))),
                (1, RowKind::Project(ProjectId(0))),
                (2, RowKind::Worktree(WorktreeId(0))),
                (1, RowKind::Project(ProjectId(1))),
                (2, RowKind::Worktree(WorktreeId(1))),
            ],
            "worktrees are leaves — sessions are center-pane tabs, not rows"
        );
    }

    /// Until groups exist, a project must not be nested under a row that just
    /// repeats its own name — that is what made the tree read `zed > zed`.
    #[test]
    fn test_ungrouped_projects_render_at_the_top_level() {
        let mut tree = tree();
        tree.groups[0].name = None;
        assert_eq!(
            shape(&tree),
            vec![
                (0, RowKind::Project(ProjectId(0))),
                (1, RowKind::Worktree(WorktreeId(0))),
                (0, RowKind::Project(ProjectId(1))),
                (1, RowKind::Worktree(WorktreeId(1))),
            ]
        );
    }

    #[test]
    fn test_remote_project_info_disambiguates_ssh_hosts() {
        let mut tree = ungrouped_tree();
        let project = &mut tree.groups[0].projects[0];
        project.worktrees[0].folder_root = Some(PathBuf::from("/home/user/Code/viral-studio"));
        project.worktrees[0].is_primary = true;

        let remote_key = |host: &str| {
            ProjectGroupKey::new(
                Some(remote::RemoteConnectionOptions::Ssh(
                    remote::SshConnectionOptions {
                        host: host.into(),
                        username: Some("user".to_owned()),
                        ..Default::default()
                    },
                )),
                PathList::new(&[PathBuf::from("/home/user/Code/viral-studio")]),
            )
        };
        project.worktrees[0].group_key = Some(remote_key("100.78.83.67"));

        let kind = RowKind::Project(ProjectId(0));
        assert_eq!(
            remote_project_info(&tree, &kind),
            Some("user@100.78.83.67 · /home/user/Code/viral-studio".into())
        );

        tree.groups[0].projects[0].worktrees[0].group_key = Some(remote_key("fevm1.local"));
        assert_eq!(
            remote_project_info(&tree, &kind),
            Some("user@fevm1.local · /home/user/Code/viral-studio".into())
        );
    }

    fn ungrouped_tree() -> WorkspaceTree {
        let mut tree = tree();
        tree.groups[0].name = None;
        tree
    }

    #[test]
    fn test_the_primary_worktree_sorts_above_its_siblings() {
        let primary = worktree_sort_key(true, &"main".into(), Some(&PathBuf::from("/src/project")));
        let earlier_alphabetically = worktree_sort_key(
            false,
            &"aaa-feature".into(),
            Some(&PathBuf::from("/src/worktrees/aaa")),
        );
        assert!(
            primary < earlier_alphabetically,
            "the original clone outranks a branch that sorts earlier by name"
        );

        let later = worktree_sort_key(
            false,
            &"zzz-feature".into(),
            Some(&PathBuf::from("/src/worktrees/zzz")),
        );
        assert!(
            earlier_alphabetically < later,
            "non-primary worktrees keep display-name order between themselves"
        );
    }

    /// ZOrca lays worktrees out as `worktrees/<repo>/<name>/<repo>`, so a
    /// detached worktree's leaf directory is just the repository name. The
    /// name the user chose is the parent.
    #[test]
    fn test_a_detached_worktree_is_named_by_its_directory() {
        assert_eq!(
            worktree_name(
                Some(Path::new("/private/tmp/zorca-feature-demo")),
                Some(Path::new("/private/tmp/zorca-feature-demo")),
            ),
            Some("main".into()),
            "the main checkout uses the same label as the title bar"
        );
        assert_eq!(
            worktree_name(
                Some(Path::new("/src/botfarm")),
                Some(Path::new("/src/worktrees/botfarm/dense-dale/botfarm")),
            ),
            Some("dense-dale".into()),
            "the leaf repeats the main checkout's name, so the parent is the worktree name"
        );
        assert_eq!(
            worktree_name(
                Some(Path::new("/src/botfarm")),
                Some(Path::new("/src/worktrees/feature-work")),
            ),
            Some("feature-work".into()),
            "a checkout whose leaf is distinct is named by that leaf"
        );
    }

    #[test]
    fn test_only_linked_worktrees_are_removable() {
        let main = Path::new("/src/project/.git");
        assert!(
            is_main_worktree(main, Path::new("/src/project")),
            "the checkout holding .git is the main one and Git will not remove it"
        );
        assert!(!is_main_worktree(main, Path::new("/src/worktrees/feature")));
        assert!(
            !is_main_worktree(Path::new("/src/project.git"), Path::new("/src/project")),
            "a bare repository has no main checkout, so nothing is protected"
        );
        assert!(
            !is_main_worktree(
                Path::new("/src/plain-folder"),
                Path::new("/src/plain-folder")
            ),
            "a project under no version control has no worktree to remove"
        );
    }

    #[test]
    fn test_unknown_projects_are_not_removable_worktrees() {
        let mut tree = tree();
        let project = &mut tree.groups[0].projects[0];
        project.has_repository = false;
        project.worktrees[0].folder_root = Some(PathBuf::from("/src/plain-folder"));

        assert_eq!(tree.removable_worktree_root(WorktreeId(0)), None);
    }

    #[test]
    fn test_an_empty_filter_changes_nothing() {
        let mut tree = ungrouped_tree();
        let before = shape(&tree);
        filter_tree(&mut tree, "   ");
        assert_eq!(shape(&tree), before);
    }

    #[test]
    fn test_filtering_to_a_worktree_keeps_the_project_above_it() {
        let mut tree = ungrouped_tree();
        tree.groups[0].projects[1].worktrees[0].name = "feature-worktree".into();
        filter_tree(&mut tree, "FEATURE");

        let labels: Vec<_> = tree
            .rows()
            .into_iter()
            .map(|row| row.label.to_string())
            .collect();
        assert_eq!(
            labels,
            vec!["second-project".to_owned(), "feature-worktree".to_owned()],
            "a matching worktree has to keep its project, or it would be unreachable"
        );
    }

    #[test]
    fn test_matching_a_project_keeps_all_of_its_worktrees() {
        let mut tree = ungrouped_tree();
        filter_tree(&mut tree, "first");

        let labels: Vec<_> = tree
            .rows()
            .into_iter()
            .map(|row| row.label.to_string())
            .collect();
        assert_eq!(labels, vec!["first-project".to_owned(), "main".to_owned()]);
    }

    #[test]
    fn test_filtering_drops_groups_that_match_nothing() {
        let mut tree = tree();
        filter_tree(&mut tree, "nothing-matches-this");
        assert!(tree.rows().is_empty());
    }

    #[test]
    fn test_matching_a_group_keeps_everything_under_it() {
        let mut tree = tree();
        let before = shape(&tree);
        filter_tree(&mut tree, "group");
        assert_eq!(shape(&tree), before);
    }

    fn pinned_tree() -> WorkspaceTree {
        let mut tree = ungrouped_tree();
        for (index, project) in tree.groups[0].projects.iter_mut().enumerate() {
            for worktree in &mut project.worktrees {
                worktree.folder_root = Some(PathBuf::from(format!("/src/checkout-{index}")));
            }
        }
        tree
    }

    fn same_path_on_three_hosts_tree() -> WorkspaceTree {
        let mut local = worktree(0, "local");
        local.folder_root = Some(PathBuf::from("/same"));
        local.project_key = Path::new("/same/.git").into();
        local.group_key = Some(ProjectGroupKey::new(
            None,
            PathList::new(&[PathBuf::from("/same")]),
        ));
        let mut remote_a = worktree(1, "remote-a");
        remote_a.folder_root = Some(PathBuf::from("/same"));
        remote_a.project_key = Path::new("/same/.git").into();
        remote_a.group_key = Some(remote_key(1, "/same"));
        let mut remote_b = worktree(2, "remote-b");
        remote_b.folder_root = Some(PathBuf::from("/same"));
        remote_b.project_key = Path::new("/same/.git").into();
        remote_b.group_key = Some(remote_key(2, "/same"));
        WorkspaceTree {
            pinned: Vec::new(),
            pinned_collapsed: false,
            groups: vec![Group {
                id: GroupId(0),
                name: None,
                collapsed: false,
                projects: vec![
                    Project {
                        id: ProjectId(0),
                        key: Path::new("/same/.git").into(),
                        name: "local".into(),
                        has_repository: true,
                        group_keys: Vec::new(),
                        collapsed: false,
                        worktrees: vec![local],
                    },
                    Project {
                        id: ProjectId(1),
                        key: Path::new("/same/.git").into(),
                        name: "remote-a".into(),
                        has_repository: true,
                        group_keys: Vec::new(),
                        collapsed: false,
                        worktrees: vec![remote_a],
                    },
                    Project {
                        id: ProjectId(2),
                        key: Path::new("/same/.git").into(),
                        name: "remote-b".into(),
                        has_repository: true,
                        group_keys: Vec::new(),
                        collapsed: false,
                        worktrees: vec![remote_b],
                    },
                ],
            }],
        }
    }

    #[test]
    fn test_scoped_state_does_not_leak_between_same_paths_on_hosts() {
        let host_a = host_cache_key(remote_key(1, "/same").host().as_ref());
        let scoped_root = ScopedPath::new(PathBuf::from("/same"), host_a.clone());
        let scoped_project = ScopedPath::new(PathBuf::from("/same/.git"), host_a);

        let mut unread = same_path_on_three_hosts_tree();
        apply_unread(&mut unread, std::slice::from_ref(&scoped_root));
        assert_eq!(
            unread.groups[0]
                .projects
                .iter()
                .map(|project| project.worktrees[0].is_unread)
                .collect::<Vec<_>>(),
            vec![false, true, false]
        );

        let mut pinned = same_path_on_three_hosts_tree();
        apply_pins(&mut pinned, std::slice::from_ref(&scoped_root));
        assert_eq!(
            pinned
                .pinned
                .iter()
                .map(|worktree| worktree.name.as_ref())
                .collect::<Vec<_>>(),
            vec!["remote-a"]
        );

        let mut hidden = same_path_on_three_hosts_tree();
        apply_hidden_worktrees(
            &mut hidden,
            &HashSet::from([scoped_root.clone()]),
            &HashSet::new(),
        );
        assert_eq!(hidden.groups[0].projects[0].worktrees.len(), 1);
        assert_eq!(hidden.groups[0].projects[1].worktrees.len(), 0);
        assert_eq!(hidden.groups[0].projects[2].worktrees.len(), 1);

        let mut revealed = same_path_on_three_hosts_tree();
        apply_hidden_worktrees(
            &mut revealed,
            &HashSet::from([
                scoped_root.clone(),
                ScopedPath::new(
                    PathBuf::from("/same"),
                    host_cache_key(remote_key(2, "/same").host().as_ref()),
                ),
            ]),
            &HashSet::from([scoped_project.clone()]),
        );
        assert_eq!(revealed.groups[0].projects[1].worktrees.len(), 1);
        assert_eq!(revealed.groups[0].projects[2].worktrees.len(), 0);

        let mut grouped = same_path_on_three_hosts_tree();
        apply_groups(&mut grouped, &[("chosen".into(), vec![scoped_project])]);
        assert_eq!(grouped.groups[0].projects.len(), 1);
        assert_eq!(grouped.groups[0].projects[0].name.as_ref(), "remote-a");
    }

    #[test]
    fn test_legacy_state_resolves_only_for_an_unambiguous_host() {
        let legacy = ScopedPath::Legacy(PathBuf::from("/same"));
        assert_eq!(
            legacy.resolved(&HashSet::from([(
                PathBuf::from("/same"),
                Some("mock:1".to_owned()),
            )])),
            Some(ScopedPath::new(
                PathBuf::from("/same"),
                Some("mock:1".to_owned()),
            ))
        );
        assert_eq!(
            legacy.resolved(&HashSet::from([
                (PathBuf::from("/same"), Some("mock:1".to_owned())),
                (PathBuf::from("/same"), Some("mock:2".to_owned())),
            ])),
            None
        );
    }

    #[test]
    fn test_unread_marks_only_the_named_worktree() {
        let mut tree = pinned_tree();
        apply_unread(&mut tree, &[PathBuf::from("/src/checkout-1").into()]);

        let unread: Vec<_> = tree
            .groups
            .iter()
            .flat_map(|group| group.projects.iter())
            .flat_map(|project| project.worktrees.iter())
            .map(|worktree| worktree.is_unread)
            .collect();
        assert_eq!(unread, vec![false, true]);
    }

    #[test]
    fn test_unread_reaches_a_pinned_worktree_too() {
        let mut tree = pinned_tree();
        apply_pins(&mut tree, &[PathBuf::from("/src/checkout-0").into()]);
        apply_unread(&mut tree, &[PathBuf::from("/src/checkout-0").into()]);

        assert!(
            tree.pinned.iter().all(|worktree| worktree.is_unread),
            "pinning lifts a worktree out of its project, and unread must follow it there"
        );
    }

    #[test]
    fn test_a_running_worktree_keeps_its_status_colour_when_unread() {
        let mut tree = pinned_tree();
        tree.groups[0].projects[0].worktrees[0].status = WorktreeStatus::Active;
        apply_unread(&mut tree, &[PathBuf::from("/src/checkout-0").into()]);

        let worktree = &tree.groups[0].projects[0].worktrees[0];
        assert!(worktree.is_unread);
        assert_eq!(
            worktree.status.indicator_color(),
            Color::Success,
            "unread must not mask a worktree that is actually running"
        );
    }

    #[test]
    fn test_pinning_nothing_leaves_the_tree_untouched() {
        let mut tree = pinned_tree();
        let before = shape(&tree);
        apply_pins(&mut tree, &[]);
        assert_eq!(shape(&tree), before);
    }

    #[test]
    fn test_a_pinned_worktree_leaves_its_project_for_the_pinned_section() {
        let mut tree = pinned_tree();
        apply_pins(&mut tree, &[PathBuf::from("/src/checkout-0").into()]);

        let labels: Vec<_> = tree
            .rows()
            .into_iter()
            .map(|row| (row.depth, row.label.to_string()))
            .collect();
        assert_eq!(
            labels,
            vec![
                (0, "Pinned".to_owned()),
                (1, "main".to_owned()),
                (0, "second-project".to_owned()),
                (1, "main".to_owned()),
            ],
            "Orca's default policy is single-location: the pinned worktree \
             leaves its project rather than appearing in both places"
        );
    }

    #[test]
    fn test_a_project_emptied_by_pinning_does_not_linger() {
        let mut tree = pinned_tree();
        apply_pins(
            &mut tree,
            &[
                PathBuf::from("/src/checkout-0").into(),
                PathBuf::from("/src/checkout-1").into(),
            ],
        );
        let labels: Vec<_> = tree
            .rows()
            .into_iter()
            .map(|row| row.label.to_string())
            .collect();
        assert_eq!(
            labels,
            vec!["Pinned".to_owned(), "main".to_owned(), "main".to_owned()],
            "a project with every worktree pinned would otherwise render empty"
        );
    }

    #[test]
    fn test_the_pinned_section_collapses() {
        let mut tree = pinned_tree();
        apply_pins(&mut tree, &[PathBuf::from("/src/checkout-0").into()]);
        tree.toggle_collapsed(&RowKind::PinnedSection);
        assert_eq!(
            tree.rows()
                .into_iter()
                .map(|row| row.label.to_string())
                .collect::<Vec<_>>(),
            vec![
                "Pinned".to_owned(),
                "second-project".to_owned(),
                "main".to_owned()
            ]
        );
    }

    #[test]
    fn test_grouping_nothing_leaves_the_tree_untouched() {
        let mut tree = ungrouped_tree();
        let before = shape(&tree);
        apply_groups(&mut tree, &[]);
        assert_eq!(shape(&tree), before);
    }

    #[test]
    fn test_a_group_claims_its_project_and_leaves_the_rest_at_the_top() {
        let mut tree = ungrouped_tree();
        apply_groups(
            &mut tree,
            &[(
                "my-group".into(),
                vec![Path::new("/src/first-project/.git").into()],
            )],
        );

        let labels: Vec<_> = tree
            .rows()
            .into_iter()
            .map(|row| (row.depth, row.label.to_string()))
            .collect();
        assert_eq!(
            labels,
            vec![
                (0, "my-group".to_owned()),
                (1, "first-project".to_owned()),
                (2, "main".to_owned()),
                (0, "second-project".to_owned()),
                (1, "main".to_owned()),
            ]
        );
    }

    #[test]
    fn test_a_project_lands_in_only_the_first_group_that_claims_it() {
        let mut tree = ungrouped_tree();
        let key: ScopedPath = Path::new("/src/first-project/.git").into();
        apply_groups(
            &mut tree,
            &[
                ("first-group".into(), vec![key.clone()]),
                ("second-group".into(), vec![key]),
            ],
        );

        let claimed: Vec<_> = tree
            .groups
            .iter()
            .map(|group| (group.name.clone(), group.projects.len()))
            .collect();
        assert_eq!(
            claimed,
            vec![
                (Some("first-group".into()), 1),
                (Some("second-group".into()), 0),
                (None, 1),
            ],
            "a project listed in two groups must not be duplicated into both"
        );
    }

    #[test]
    fn test_project_name_comes_from_the_main_worktree() {
        assert_eq!(
            project_name(Path::new("/src/some-project/.git")),
            "some-project",
            "a linked worktree's common dir names the repository, not the worktree"
        );
        assert_eq!(
            project_name(Path::new("/src/some-project.git")),
            "some-project",
            "a bare repository has no main worktree to take a name from"
        );
    }

    #[test]
    fn test_collapsing_hides_only_the_subtree() {
        let mut tree = tree();
        tree.toggle_collapsed(&RowKind::Project(ProjectId(0)));
        assert_eq!(
            shape(&tree),
            vec![
                (0, RowKind::Group(GroupId(0))),
                (1, RowKind::Project(ProjectId(0))),
                (1, RowKind::Project(ProjectId(1))),
                (2, RowKind::Worktree(WorktreeId(1))),
            ],
            "collapsing a project must hide its worktrees without touching its sibling"
        );
    }

    #[test]
    fn test_collapsed_group_hides_every_descendant() {
        let mut tree = tree();
        tree.toggle_collapsed(&RowKind::Group(GroupId(0)));
        assert_eq!(shape(&tree), vec![(0, RowKind::Group(GroupId(0)))]);
    }

    /// The invariant the old sidebar needed a property test to defend: no row
    /// is visible unless every ancestor above it is expanded.
    #[test]
    fn test_every_visible_row_is_reachable_from_a_root() {
        let mut tree = tree();
        tree.toggle_collapsed(&RowKind::Project(ProjectId(0)));

        let mut expanded_depth = 0;
        for row in &tree.rows() {
            assert!(
                row.depth <= expanded_depth,
                "row {:?} at depth {} appeared under a collapsed ancestor",
                row.kind,
                row.depth
            );
            expanded_depth = row.depth + 1;
        }
    }

    #[test]
    fn test_disclosure_shown_only_where_there_are_children() {
        let tree = tree();
        assert_eq!(tree.is_collapsed(&RowKind::Group(GroupId(0))), Some(false));
        assert_eq!(
            tree.is_collapsed(&RowKind::Project(ProjectId(0))),
            Some(false)
        );
        assert_eq!(
            tree.is_collapsed(&RowKind::Worktree(WorktreeId(0))),
            None,
            "a worktree is a leaf and must not render a disclosure triangle"
        );
    }

    #[test]
    fn test_toggling_a_worktree_row_is_a_noop() {
        let mut tree = tree();
        let before = shape(&tree);
        tree.toggle_collapsed(&RowKind::Worktree(WorktreeId(0)));
        assert_eq!(shape(&tree), before);
    }

    #[test]
    fn test_hidden_worktrees_can_be_revealed_per_project() {
        let mut tree = tree();
        let root = PathBuf::from("/src/first-project-feature");
        tree.groups[0].projects[0].worktrees[0].folder_root = Some(root.clone());
        let hidden = HashSet::from([root.into()]);

        let mut hidden_tree = tree.clone();
        apply_hidden_worktrees(&mut hidden_tree, &hidden, &HashSet::new());
        assert!(hidden_tree.groups[0].projects[0].worktrees.is_empty());

        apply_hidden_worktrees(
            &mut tree,
            &hidden,
            &HashSet::from([PathBuf::from("/src/first-project/.git").into()]),
        );
        assert_eq!(tree.groups[0].projects[0].worktrees.len(), 1);
    }

    #[test]
    fn test_worktree_status_maps_to_indicator_color() {
        assert_eq!(WorktreeStatus::Inactive.indicator_color(), Color::Muted);
        assert_eq!(WorktreeStatus::Active.indicator_color(), Color::Success);
    }
}
