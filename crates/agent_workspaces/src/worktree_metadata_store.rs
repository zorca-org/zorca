//! Persistence for archived git worktrees, and worktree display names.
//!
//! Split out of `thread_metadata_store` because it outlives it: archiving a
//! worktree is a workspace operation the sidebar drives, not a thread
//! operation, and stage 5 deletes the thread surfaces around it.

use std::path::PathBuf;

use anyhow::Context as _;
use collections::HashSet;
use db::{
    sqlez::{
        bindable::Column, domain::Domain, statement::Statement,
        thread_safe_connection::ThreadSafeConnection,
    },
    sqlez_macros::sql,
};
use gpui::{App, AppContext as _, Entity, Global, Task};
use project::{WorktreePaths, linked_worktree_short_name};
use ui::{SharedString, ThreadItemWorktreeInfo, WorktreeKind};

/// Derives worktree display info from a stored path list.
///
/// For each path in `folder_paths`, produces a [`ThreadItemWorktreeInfo`] with
/// a short display name, full path, and whether the worktree is the main
/// checkout or a linked git worktree. When multiple main paths exist and a
/// linked worktree's short name alone wouldn't identify which main project it
/// belongs to, the main project name is prefixed for disambiguation
/// (e.g. `project:feature`).
pub fn worktree_info_from_thread_paths<S: std::hash::BuildHasher>(
    worktree_paths: &WorktreePaths,
    branch_names: &std::collections::HashMap<PathBuf, SharedString, S>,
) -> Vec<ThreadItemWorktreeInfo> {
    let mut infos: Vec<ThreadItemWorktreeInfo> = Vec::new();
    let mut linked_short_names: Vec<(SharedString, SharedString)> = Vec::new();
    let mut unique_main_count = HashSet::default();

    for (main_path, folder_path) in worktree_paths.ordered_pairs() {
        unique_main_count.insert(main_path.clone());
        let is_linked = main_path != folder_path;

        if is_linked {
            let short_name = linked_worktree_short_name(main_path, folder_path).unwrap_or_default();
            let project_name = main_path
                .file_name()
                .map(|n| SharedString::from(n.to_string_lossy().to_string()))
                .unwrap_or_default();
            linked_short_names.push((short_name.clone(), project_name));
            infos.push(ThreadItemWorktreeInfo {
                worktree_name: Some(short_name),
                full_path: SharedString::from(folder_path.display().to_string()),
                highlight_positions: Vec::new(),
                kind: WorktreeKind::Linked,
                branch_name: branch_names.get(folder_path).cloned(),
            });
        } else {
            let Some(name) = folder_path.file_name() else {
                continue;
            };
            infos.push(ThreadItemWorktreeInfo {
                worktree_name: Some(SharedString::from(name.to_string_lossy().to_string())),
                full_path: SharedString::from(folder_path.display().to_string()),
                highlight_positions: Vec::new(),
                kind: WorktreeKind::Main,
                branch_name: branch_names.get(folder_path).cloned(),
            });
        }
    }

    // When the group has multiple main worktree paths and the folder paths
    // don't all share the same short name, prefix each linked worktree chip
    // with its main project name so the user knows which project it belongs to.
    let all_same_name = infos.len() > 1
        && infos
            .iter()
            .all(|i| i.worktree_name == infos[0].worktree_name);

    if unique_main_count.len() > 1 && !all_same_name {
        for (info, (_short_name, project_name)) in infos
            .iter_mut()
            .filter(|i| i.kind == WorktreeKind::Linked)
            .zip(linked_short_names.iter())
        {
            if let Some(name) = &info.worktree_name {
                info.worktree_name = Some(SharedString::from(format!("{}:{}", project_name, name)));
            }
        }
    }

    infos
}

/// Record of a git worktree that was archived (deleted from disk).
pub struct ArchivedGitWorktree {
    /// Auto-incrementing primary key.
    pub id: i64,
    /// Absolute path to the directory of the worktree before it was deleted.
    /// Used when restoring, to put the recreated worktree back where it was.
    /// If the path already exists on disk, the worktree is assumed to be
    /// already restored and is used as-is.
    pub worktree_path: PathBuf,
    /// Absolute path of the main repository ("main worktree") that owned this worktree.
    /// Used when restoring, to reattach the recreated worktree to the correct main repo.
    /// If the main repo isn't found on disk, unarchiving fails because we only store
    /// commit hashes, and without the actual git repo being available, we can't restore
    /// the files.
    pub main_repo_path: PathBuf,
    /// Branch that was checked out in the worktree at archive time. `None` if
    /// the worktree was in detached HEAD state, which isn't supported here, but
    /// could happen if the user made a detached one outside the app.
    /// On restore, we try to switch to this branch. If that fails (e.g. it's
    /// checked out elsewhere), we auto-generate a new one.
    pub branch_name: Option<String>,
    /// SHA of the WIP commit that captures files that were staged (but not yet
    /// committed) at the time of archiving. This commit can be empty if the
    /// user had no staged files at the time. It sits directly on top of whatever
    /// the user's last actual commit was.
    pub staged_commit_hash: String,
    /// SHA of the WIP commit that captures files that were unstaged (including
    /// untracked) at the time of archiving. This commit can be empty if the user
    /// had no unstaged files at the time. It sits on top of `staged_commit_hash`.
    /// After doing `git reset` past both of these commits, we're back in the state
    /// we had before archiving, including what was staged, what was unstaged, and
    /// what was committed.
    pub unstaged_commit_hash: String,
    /// SHA of the commit that HEAD pointed at before we created the two WIP
    /// commits during archival. After resetting past the WIP commits during
    /// restore, HEAD should land back on this commit. It also serves as a
    /// pre-restore sanity check (abort if this commit no longer exists in the
    /// repo) and as a fallback target if the WIP resets fail.
    pub original_commit_hash: String,
}

impl Column for ArchivedGitWorktree {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (id, next): (i64, i32) = Column::column(statement, start_index)?;
        let (worktree_path_str, next): (String, i32) = Column::column(statement, next)?;
        let (main_repo_path_str, next): (String, i32) = Column::column(statement, next)?;
        let (branch_name, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (staged_commit_hash, next): (String, i32) = Column::column(statement, next)?;
        let (unstaged_commit_hash, next): (String, i32) = Column::column(statement, next)?;
        let (original_commit_hash, next): (String, i32) = Column::column(statement, next)?;

        Ok((
            ArchivedGitWorktree {
                id,
                worktree_path: PathBuf::from(worktree_path_str),
                main_repo_path: PathBuf::from(main_repo_path_str),
                branch_name,
                staged_commit_hash,
                unstaged_commit_hash,
                original_commit_hash,
            },
            next,
        ))
    }
}

pub struct WorktreeMetadataDb(pub ThreadSafeConnection);

impl Domain for WorktreeMetadataDb {
    // Pinned to the old struct name on purpose: sqlez keys the applied-migration
    // log on this string, so changing it would replay every migration against a
    // database that already has the tables.
    const NAME: &str = "ThreadMetadataDb";

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE IF NOT EXISTS sidebar_threads(
                session_id TEXT PRIMARY KEY,
                agent_id TEXT,
                title TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                created_at TEXT,
                folder_paths TEXT,
                folder_paths_order TEXT
            ) STRICT;
        ),
        sql!(ALTER TABLE sidebar_threads ADD COLUMN archived INTEGER DEFAULT 0),
        sql!(ALTER TABLE sidebar_threads ADD COLUMN main_worktree_paths TEXT),
        sql!(ALTER TABLE sidebar_threads ADD COLUMN main_worktree_paths_order TEXT),
        sql!(
            CREATE TABLE IF NOT EXISTS archived_git_worktrees(
                id INTEGER PRIMARY KEY,
                worktree_path TEXT NOT NULL,
                main_repo_path TEXT NOT NULL,
                branch_name TEXT,
                staged_commit_hash TEXT,
                unstaged_commit_hash TEXT,
                original_commit_hash TEXT
            ) STRICT;

            CREATE TABLE IF NOT EXISTS thread_archived_worktrees(
                session_id TEXT NOT NULL,
                archived_worktree_id INTEGER NOT NULL REFERENCES archived_git_worktrees(id),
                PRIMARY KEY (session_id, archived_worktree_id)
            ) STRICT;
        ),
        sql!(ALTER TABLE sidebar_threads ADD COLUMN remote_connection TEXT),
        sql!(ALTER TABLE sidebar_threads ADD COLUMN thread_id BLOB),
        sql!(
            UPDATE sidebar_threads SET thread_id = randomblob(16) WHERE thread_id IS NULL;

            CREATE TABLE thread_archived_worktrees_v2(
                thread_id BLOB NOT NULL,
                archived_worktree_id INTEGER NOT NULL REFERENCES archived_git_worktrees(id),
                PRIMARY KEY (thread_id, archived_worktree_id)
            ) STRICT;

            INSERT INTO thread_archived_worktrees_v2(thread_id, archived_worktree_id)
            SELECT s.thread_id, t.archived_worktree_id
            FROM thread_archived_worktrees t
            JOIN sidebar_threads s ON s.session_id = t.session_id;

            DROP TABLE thread_archived_worktrees;
            ALTER TABLE thread_archived_worktrees_v2 RENAME TO thread_archived_worktrees;

            CREATE TABLE sidebar_threads_v2(
                thread_id BLOB PRIMARY KEY,
                session_id TEXT,
                agent_id TEXT,
                title TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                created_at TEXT,
                folder_paths TEXT,
                folder_paths_order TEXT,
                archived INTEGER DEFAULT 0,
                main_worktree_paths TEXT,
                main_worktree_paths_order TEXT,
                remote_connection TEXT
            ) STRICT;

            INSERT INTO sidebar_threads_v2(thread_id, session_id, agent_id, title, updated_at, created_at, folder_paths, folder_paths_order, archived, main_worktree_paths, main_worktree_paths_order, remote_connection)
            SELECT thread_id, session_id, agent_id, title, updated_at, created_at, folder_paths, folder_paths_order, archived, main_worktree_paths, main_worktree_paths_order, remote_connection
            FROM sidebar_threads;

            DROP TABLE sidebar_threads;
            ALTER TABLE sidebar_threads_v2 RENAME TO sidebar_threads;
        ),
        sql!(
            DELETE FROM thread_archived_worktrees
            WHERE thread_id IN (
                SELECT thread_id FROM sidebar_threads WHERE session_id IS NULL
            );

            DELETE FROM sidebar_threads WHERE session_id IS NULL;

            DELETE FROM archived_git_worktrees
            WHERE id NOT IN (
                SELECT archived_worktree_id FROM thread_archived_worktrees
            );
        ),
        sql!(
            ALTER TABLE sidebar_threads ADD COLUMN interacted_at TEXT;
        ),
        sql!(
            ALTER TABLE sidebar_threads ADD COLUMN title_override TEXT;
        ),
    ];
}

db::static_connection!(WorktreeMetadataDb, []);

impl WorktreeMetadataDb {
    pub async fn create_archived_worktree(
        &self,
        worktree_path: String,
        main_repo_path: String,
        branch_name: Option<String>,
        staged_commit_hash: String,
        unstaged_commit_hash: String,
        original_commit_hash: String,
    ) -> anyhow::Result<i64> {
        self.write(move |conn| {
            let mut stmt = Statement::prepare(
                conn,
                "INSERT INTO archived_git_worktrees(worktree_path, main_repo_path, branch_name, staged_commit_hash, unstaged_commit_hash, original_commit_hash) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 RETURNING id",
            )?;
            let mut i = stmt.bind(&worktree_path, 1)?;
            i = stmt.bind(&main_repo_path, i)?;
            i = stmt.bind(&branch_name, i)?;
            i = stmt.bind(&staged_commit_hash, i)?;
            i = stmt.bind(&unstaged_commit_hash, i)?;
            stmt.bind(&original_commit_hash, i)?;
            stmt.maybe_row::<i64>()?.context("expected RETURNING id")
        })
        .await
    }

    pub async fn delete_archived_worktree(&self, id: i64) -> anyhow::Result<()> {
        self.write(move |conn| {
            let mut stmt = Statement::prepare(
                conn,
                "DELETE FROM thread_archived_worktrees WHERE archived_worktree_id = ?",
            )?;
            stmt.bind(&id, 1)?;
            stmt.exec()?;

            let mut stmt =
                Statement::prepare(conn, "DELETE FROM archived_git_worktrees WHERE id = ?")?;
            stmt.bind(&id, 1)?;
            stmt.exec()
        })
        .await
    }

    pub async fn is_archived_worktree_referenced(
        &self,
        archived_worktree_id: i64,
    ) -> anyhow::Result<bool> {
        self.select_row_bound::<i64, i64>(
            "SELECT COUNT(*) FROM thread_archived_worktrees WHERE archived_worktree_id = ?1",
        )?(archived_worktree_id)
        .map(|count| count.unwrap_or(0) > 0)
    }
}

/// Override for the test DB name used by [`WorktreeMetadataStore::init_global`]
/// and `ThreadMetadataStore::init_global`. When set as a GPUI global, both use
/// this name instead of deriving one from the thread name. This prevents data
/// from leaking across proptest cases that share a thread name.
#[cfg(any(test, feature = "test-support"))]
pub struct TestMetadataDbName(pub String);
#[cfg(any(test, feature = "test-support"))]
impl gpui::Global for TestMetadataDbName {}

#[cfg(any(test, feature = "test-support"))]
impl TestMetadataDbName {
    pub fn global(cx: &App) -> String {
        cx.try_global::<Self>()
            .map(|g| g.0.clone())
            .unwrap_or_else(|| {
                let thread = std::thread::current();
                let test_name = thread.name().unwrap_or("unknown_test");
                format!("THREAD_METADATA_DB_{}", test_name)
            })
    }
}

/// Thread-agnostic access to the archived-worktree tables.
///
/// Exists as an entity rather than a bare `WorktreeMetadataDb::global` so tests
/// can point it at a scratch database.
pub struct WorktreeMetadataStore {
    db: WorktreeMetadataDb,
}

struct GlobalWorktreeMetadataStore(Entity<WorktreeMetadataStore>);
impl Global for GlobalWorktreeMetadataStore {}

impl WorktreeMetadataStore {
    #[cfg(not(any(test, feature = "test-support")))]
    pub fn init_global(cx: &mut App) {
        if cx.has_global::<GlobalWorktreeMetadataStore>() {
            return;
        }
        let db = WorktreeMetadataDb::global(cx);
        let store = cx.new(|_| Self { db });
        cx.set_global(GlobalWorktreeMetadataStore(store));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn init_global(cx: &mut App) {
        let db_name = TestMetadataDbName::global(cx);
        let db = gpui::block_on(db::open_test_db::<WorktreeMetadataDb>(&db_name));
        let store = cx.new(|_| Self {
            db: WorktreeMetadataDb(db),
        });
        cx.set_global(GlobalWorktreeMetadataStore(store));
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalWorktreeMetadataStore>()
            .map(|store| store.0.clone())
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalWorktreeMetadataStore>().0.clone()
    }

    pub fn create_archived_worktree(
        &self,
        worktree_path: String,
        main_repo_path: String,
        branch_name: Option<String>,
        staged_commit_hash: String,
        unstaged_commit_hash: String,
        original_commit_hash: String,
        cx: &App,
    ) -> Task<anyhow::Result<i64>> {
        let db = self.db.clone();
        cx.background_spawn(async move {
            db.create_archived_worktree(
                worktree_path,
                main_repo_path,
                branch_name,
                staged_commit_hash,
                unstaged_commit_hash,
                original_commit_hash,
            )
            .await
        })
    }

    pub fn delete_archived_worktree(&self, id: i64, cx: &App) -> Task<anyhow::Result<()>> {
        let db = self.db.clone();
        cx.background_spawn(async move { db.delete_archived_worktree(id).await })
    }

    pub fn is_archived_worktree_referenced(
        &self,
        archived_worktree_id: i64,
        cx: &App,
    ) -> Task<anyhow::Result<bool>> {
        let db = self.db.clone();
        cx.background_spawn(async move {
            db.is_archived_worktree_referenced(archived_worktree_id)
                .await
        })
    }
}
