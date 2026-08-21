use crate::{AdeWorkspace, WorkspaceId, WorkspaceStatus};
use anyhow::Result;
use db::{
    query,
    sqlez::{domain::Domain, statement::Statement, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use time::OffsetDateTime;

/// Durable metadata store for [`AdeWorkspace`]s, in Zed's shared sqlite
/// database.
///
/// This is a *cache*: tmux owns the truth about whether a session is alive.
/// Nothing here talks to tmux, ssh, or git — callers reconcile.
pub struct AdeWorkspaceRegistry(ThreadSafeConnection);

impl Domain for AdeWorkspaceRegistry {
    const NAME: &str = stringify!(AdeWorkspaceRegistry);

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE IF NOT EXISTS ade_workspaces(
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                project_id TEXT NOT NULL,
                repository_path BLOB NOT NULL,
                branch TEXT,
                remote_host TEXT, // NULL means the workspace is local
                remote_workspace_path BLOB,
                terminal_session_id TEXT,
                status TEXT NOT NULL,
                created_at INTEGER NOT NULL, // unix seconds
                last_opened_at INTEGER NOT NULL // unix seconds
            ) STRICT;
        ),
        // Migration SQL is persisted verbatim; append changes instead of
        // editing this step.
        sql!(ALTER TABLE ade_workspaces ADD COLUMN used_by_client_at INTEGER;),
    ];
}

db::static_connection!(AdeWorkspaceRegistry, []);

/// Column order, matching the `Bind`/`Column` impls for [`AdeWorkspace`].
const COLUMNS: &str = "id,
    name,
    project_id,
    repository_path,
    branch,
    remote_host,
    remote_workspace_path,
    terminal_session_id,
    status,
    created_at,
    last_opened_at";

impl AdeWorkspaceRegistry {
    /// Inserts a workspace, replacing any row with the same id.
    pub async fn create_workspace(&self, workspace: AdeWorkspace) -> Result<()> {
        let query = format!(
            "INSERT OR REPLACE INTO ade_workspaces ({COLUMNS}, used_by_client_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
        );
        self.write(move |connection| {
            let mut statement = Statement::prepare(connection, &query)?;
            let next_index = statement.bind(&workspace, 1)?;
            statement.bind(&crate::now_whole_seconds().unix_timestamp(), next_index)?;
            statement.exec()
        })
        .await
    }

    query! {
        pub fn get_workspace(id: WorkspaceId) -> Result<Option<AdeWorkspace>> {
            SELECT
                id,
                name,
                project_id,
                repository_path,
                branch,
                remote_host,
                remote_workspace_path,
                terminal_session_id,
                status,
                created_at,
                last_opened_at
            FROM ade_workspaces
            WHERE id = ?
        }
    }

    query! {
        pub fn list_workspaces() -> Result<Vec<AdeWorkspace>> {
            SELECT
                id,
                name,
                project_id,
                repository_path,
                branch,
                remote_host,
                remote_workspace_path,
                terminal_session_id,
                status,
                created_at,
                last_opened_at
            FROM ade_workspaces
            ORDER BY last_opened_at DESC
        }
    }

    query! {
        pub fn list_workspaces_for_project(project_id: String) -> Result<Vec<AdeWorkspace>> {
            SELECT
                id,
                name,
                project_id,
                repository_path,
                branch,
                remote_host,
                remote_workspace_path,
                terminal_session_id,
                status,
                created_at,
                last_opened_at
            FROM ade_workspaces
            WHERE project_id = ?
            ORDER BY last_opened_at DESC
        }
    }

    query! {
        pub async fn update_status(id: WorkspaceId, status: WorkspaceStatus) -> Result<()> {
            UPDATE ade_workspaces SET status = ?2 WHERE id = ?1
        }
    }

    // Renames a workspace. The id is untouched — a name is display metadata,
    // and everything else (the session, the daemon's workspace record, this
    // row) is keyed by the id. See `AdeWorkspace::daemon_workspace_id`.
    query! {
        pub async fn update_name(id: WorkspaceId, name: String) -> Result<()> {
            UPDATE ade_workspaces SET name = ?2 WHERE id = ?1
        }
    }

    query! {
        pub async fn update_terminal_session_id(
            id: WorkspaceId,
            terminal_session_id: Option<String>
        ) -> Result<()> {
            UPDATE ade_workspaces SET terminal_session_id = ?2 WHERE id = ?1
        }
    }

    query! {
        pub async fn update_remote_host(id: WorkspaceId, remote_host: Option<String>) -> Result<()> {
            UPDATE ade_workspaces SET remote_host = ?2 WHERE id = ?1
        }
    }

    /// Marks the workspace as opened at `last_opened_at` (truncated to whole
    /// seconds, which is the persisted resolution).
    pub async fn update_last_opened_at(
        &self,
        id: WorkspaceId,
        last_opened_at: OffsetDateTime,
    ) -> Result<()> {
        self.update_last_opened_at_internal(id, last_opened_at.unix_timestamp())
            .await
    }

    query! {
        async fn update_last_opened_at_internal(id: WorkspaceId, last_opened_at: i64) -> Result<()> {
            UPDATE ade_workspaces SET last_opened_at = ?2 WHERE id = ?1
        }
    }

    query! {
        pub async fn delete_workspace(id: WorkspaceId) -> Result<()> {
            DELETE FROM ade_workspaces WHERE id = ?
        }
    }
}

#[cfg(test)]
impl AdeWorkspaceRegistry {
    query! {
        fn used_by_client_at(id: WorkspaceId) -> Result<Option<i64>> {
            SELECT used_by_client_at FROM ade_workspaces WHERE id = ?
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct BeforeUsageProvenance;

    impl Domain for BeforeUsageProvenance {
        const NAME: &str = <AdeWorkspaceRegistry as Domain>::NAME;
        const MIGRATIONS: &[&str] = &[<AdeWorkspaceRegistry as Domain>::MIGRATIONS[0]];
    }

    fn workspace(name: &str, project_id: &str) -> AdeWorkspace {
        AdeWorkspace::new(name, project_id, "/repos/zed")
    }

    async fn insert_without_usage_column(
        connection: &ThreadSafeConnection,
        workspace: AdeWorkspace,
    ) {
        let query = format!(
            "INSERT OR REPLACE INTO ade_workspaces ({COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
        );
        connection
            .write(move |connection| {
                let mut statement = Statement::prepare(connection, &query)?;
                statement.bind(&workspace, 1)?;
                statement.exec()
            })
            .await
            .unwrap();
    }

    #[gpui::test]
    async fn test_migration_and_rolled_back_writes_stay_visible() {
        let name = "test_usage_provenance_rollback";
        let legacy = db::open_test_db::<BeforeUsageProvenance>(name).await;
        let before_upgrade = workspace("before-upgrade", "project-a");
        insert_without_usage_column(&legacy, before_upgrade.clone()).await;

        let db = AdeWorkspaceRegistry::open_test_db(name).await;
        assert_eq!(
            db.get_workspace(before_upgrade.id.clone()).unwrap(),
            Some(before_upgrade)
        );

        let after_rollback = workspace("after-rollback", "project-a");
        insert_without_usage_column(&legacy, after_rollback.clone()).await;
        assert_eq!(
            db.get_workspace(after_rollback.id.clone()).unwrap(),
            Some(after_rollback)
        );

        let created_here = workspace("created-here", "project-a");
        db.create_workspace(created_here.clone()).await.unwrap();
        assert!(
            db.used_by_client_at(created_here.id)
                .unwrap()
                .is_some_and(|timestamp| timestamp > 0)
        );
    }

    #[gpui::test]
    async fn test_workspace_round_trip() {
        let db = AdeWorkspaceRegistry::open_test_db("test_workspace_round_trip").await;

        let mut local = workspace("main", "project-a");
        local.branch = Some("main".into());
        local.terminal_session_id = Some(local.tmux_session_name());
        local.status = WorkspaceStatus::Running;

        let mut remote = workspace("feature/auth", "project-a");
        remote.remote_host = Some("dev-box".into());
        remote.remote_workspace_path = Some(PathBuf::from("/home/kingii/zed"));

        assert!(db.get_workspace(local.id.clone()).unwrap().is_none());

        db.create_workspace(local.clone()).await.unwrap();
        db.create_workspace(remote.clone()).await.unwrap();

        // Every field survives the trip, including the optional ones left None.
        assert_eq!(
            db.get_workspace(local.id.clone()).unwrap(),
            Some(local.clone())
        );
        assert_eq!(
            db.get_workspace(remote.id.clone()).unwrap(),
            Some(remote.clone())
        );

        let other_project = workspace("main", "project-b");
        db.create_workspace(other_project.clone()).await.unwrap();

        assert_eq!(db.list_workspaces().unwrap().len(), 3);
        let for_project_a = db.list_workspaces_for_project("project-a".into()).unwrap();
        assert_eq!(for_project_a.len(), 2);
        assert!(for_project_a.iter().all(|w| w.project_id == "project-a"));
        assert_eq!(
            db.list_workspaces_for_project("project-c".into())
                .unwrap()
                .len(),
            0
        );

        db.update_status(remote.id.clone(), WorkspaceStatus::Disconnected)
            .await
            .unwrap();
        db.update_terminal_session_id(remote.id.clone(), Some("ade-feature-auth-abc123".into()))
            .await
            .unwrap();
        let opened_at = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        db.update_last_opened_at(remote.id.clone(), opened_at)
            .await
            .unwrap();

        let stored = db.get_workspace(remote.id.clone()).unwrap().unwrap();
        assert_eq!(stored.status, WorkspaceStatus::Disconnected);
        assert_eq!(
            stored.terminal_session_id.as_deref(),
            Some("ade-feature-auth-abc123")
        );
        assert_eq!(stored.last_opened_at, opened_at);
        assert_eq!(stored.created_at, remote.created_at);
        // The updates touched only that workspace.
        assert_eq!(
            db.get_workspace(local.id.clone()).unwrap(),
            Some(local.clone())
        );

        // Clearing the session id is how a dead session is recorded.
        db.update_terminal_session_id(remote.id.clone(), None)
            .await
            .unwrap();
        assert!(
            db.get_workspace(remote.id.clone())
                .unwrap()
                .unwrap()
                .terminal_session_id
                .is_none()
        );

        db.delete_workspace(remote.id.clone()).await.unwrap();
        assert!(db.get_workspace(remote.id.clone()).unwrap().is_none());
        assert_eq!(db.list_workspaces().unwrap().len(), 2);
    }

    #[gpui::test]
    async fn test_list_is_most_recently_opened_first() {
        let db =
            AdeWorkspaceRegistry::open_test_db("test_list_is_most_recently_opened_first").await;

        let older = workspace("older", "project-a");
        let newer = workspace("newer", "project-a");
        db.create_workspace(older.clone()).await.unwrap();
        db.create_workspace(newer.clone()).await.unwrap();

        db.update_last_opened_at(
            older.id.clone(),
            OffsetDateTime::from_unix_timestamp(1_000).unwrap(),
        )
        .await
        .unwrap();
        db.update_last_opened_at(
            newer.id.clone(),
            OffsetDateTime::from_unix_timestamp(2_000).unwrap(),
        )
        .await
        .unwrap();

        let listed = db.list_workspaces().unwrap();
        assert_eq!(
            listed.iter().map(|w| w.id.clone()).collect::<Vec<_>>(),
            vec![newer.id, older.id]
        );
    }
}
