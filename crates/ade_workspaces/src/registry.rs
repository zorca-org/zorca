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
        sql!(ALTER TABLE ade_workspaces ADD COLUMN daemon_id TEXT;),
        // Collapse legacy duplicates before adding the record index. This SQL
        // was exercised by pre-split PR3 builds and must remain byte-for-byte.
        sql!(
            DELETE FROM ade_workspaces WHERE id IN (
                SELECT victim.id FROM ade_workspaces victim
                WHERE victim.used_by_client_at IS NOT NULL
                  AND victim.terminal_session_id IS NOT NULL
                  AND EXISTS (
                    SELECT winner.id FROM ade_workspaces winner
                    WHERE winner.used_by_client_at IS NOT NULL
                      AND winner.terminal_session_id = victim.terminal_session_id
                      AND winner.remote_host IS victim.remote_host
                      AND winner.id <> victim.id
                      AND (
                        (winner.branch IS NOT NULL) > (victim.branch IS NOT NULL)
                        OR ((winner.branch IS NOT NULL) = (victim.branch IS NOT NULL)
                            AND winner.last_opened_at > victim.last_opened_at)
                        OR ((winner.branch IS NOT NULL) = (victim.branch IS NOT NULL)
                            AND winner.last_opened_at = victim.last_opened_at
                            AND winner.id < victim.id)
                      )
                  )
            );
        ),
        sql!(
            CREATE UNIQUE INDEX IF NOT EXISTS ade_workspaces_one_row_per_record
            ON ade_workspaces (
                COALESCE(daemon_id, char(0) || COALESCE(remote_host, char(0))),
                terminal_session_id
            ) WHERE used_by_client_at IS NOT NULL AND terminal_session_id IS NOT NULL;
        ),
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
    daemon_id,
    status,
    created_at,
    last_opened_at";

impl AdeWorkspaceRegistry {
    /// Replaces the row with the same id, but preserves the existing owner of a
    /// conflicting daemon record so callers can re-list it without data loss.
    pub async fn create_workspace(&self, workspace: AdeWorkspace) -> Result<()> {
        let query = format!(
            "INSERT INTO ade_workspaces ({COLUMNS}, used_by_client_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT (id) DO UPDATE SET
                name = excluded.name,
                project_id = excluded.project_id,
                repository_path = excluded.repository_path,
                branch = excluded.branch,
                remote_host = excluded.remote_host,
                remote_workspace_path = excluded.remote_workspace_path,
                terminal_session_id = excluded.terminal_session_id,
                daemon_id = excluded.daemon_id,
                status = excluded.status,
                created_at = excluded.created_at,
                last_opened_at = excluded.last_opened_at,
                used_by_client_at = excluded.used_by_client_at
            -- The id clause must precede this record fence: SQLite uses the
            -- first matching conflict clause.
            ON CONFLICT (
                COALESCE(daemon_id, char(0) || COALESCE(remote_host, char(0))),
                terminal_session_id
            ) WHERE used_by_client_at IS NOT NULL AND terminal_session_id IS NOT NULL
            DO NOTHING"
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
                daemon_id,
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
                daemon_id,
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
                daemon_id,
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
        pub async fn update_terminal_session_and_daemon_id(
            id: WorkspaceId,
            terminal_session_id: Option<String>,
            daemon_id: Option<String>
        ) -> Result<()> {
            UPDATE ade_workspaces SET terminal_session_id = ?2, daemon_id = ?3 WHERE id = ?1
        }
    }

    // One statement: remote_host and daemon_id are learned from the same
    // handshake, and an interrupted write must not update one but not the
    // other.
    query! {
        pub async fn update_remote_host_and_daemon_id(
            id: WorkspaceId,
            remote_host: Option<String>,
            daemon_id: Option<String>
        ) -> Result<()> {
            UPDATE ade_workspaces SET remote_host = ?2, daemon_id = ?3 WHERE id = ?1
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

    /// Schema before duplicate cleanup and the record index.
    struct BeforeConfirmedIdentityIndex;

    impl Domain for BeforeConfirmedIdentityIndex {
        const NAME: &str = <AdeWorkspaceRegistry as Domain>::NAME;
        const MIGRATIONS: &[&str] = &[
            <AdeWorkspaceRegistry as Domain>::MIGRATIONS[0],
            <AdeWorkspaceRegistry as Domain>::MIGRATIONS[1],
            <AdeWorkspaceRegistry as Domain>::MIGRATIONS[2],
        ];
    }

    /// The table's very first column list, frozen — `COLUMNS` has grown since
    /// (`used_by_client_at`, `daemon_id`), and a pre-migration insert must
    /// target only the columns that existed then, not whatever `COLUMNS` is
    /// today.
    const ORIGINAL_COLUMNS: &str = "id,
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

    fn workspace(name: &str, project_id: &str) -> AdeWorkspace {
        AdeWorkspace::new(name, project_id, "/repos/zed")
    }

    async fn insert_without_usage_column(
        connection: &ThreadSafeConnection,
        workspace: AdeWorkspace,
    ) {
        let query = format!(
            "INSERT OR REPLACE INTO ade_workspaces ({ORIGINAL_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
        );
        connection
            .write(move |connection| {
                let mut statement = Statement::prepare(connection, &query)?;
                // Bound field-by-field, skipping `daemon_id`: the whole-struct
                // `Bind` impl emits it, and this statement's placeholders are
                // the pre-`daemon_id` schema.
                let next_index = statement.bind(&workspace.id, 1)?;
                let next_index = statement.bind(&workspace.name, next_index)?;
                let next_index = statement.bind(&workspace.project_id, next_index)?;
                let next_index = statement.bind(&workspace.repository_path, next_index)?;
                let next_index = statement.bind(&workspace.branch, next_index)?;
                let next_index = statement.bind(&workspace.remote_host, next_index)?;
                let next_index = statement.bind(&workspace.remote_workspace_path, next_index)?;
                let next_index = statement.bind(&workspace.terminal_session_id, next_index)?;
                let next_index = statement.bind(&workspace.status, next_index)?;
                let next_index =
                    statement.bind(&workspace.created_at.unix_timestamp(), next_index)?;
                statement.bind(&workspace.last_opened_at.unix_timestamp(), next_index)?;
                statement.exec()
            })
            .await
            .unwrap();
    }

    async fn insert_confirmed_row(
        connection: &ThreadSafeConnection,
        workspace: AdeWorkspace,
        used_by_client_at: Option<i64>,
    ) {
        let query = format!(
            "INSERT INTO ade_workspaces ({COLUMNS}, used_by_client_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
        );
        connection
            .write(move |connection| {
                let mut statement = Statement::prepare(connection, &query)?;
                let next_index = statement.bind(&workspace, 1)?;
                statement.bind(&used_by_client_at, next_index)?;
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
        drop(db);

        let db = AdeWorkspaceRegistry::open_test_db(name).await;
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
    async fn test_daemon_id_round_trips_and_updates_atomically_with_remote_host() {
        let db = AdeWorkspaceRegistry::open_test_db(
            "test_daemon_id_round_trips_and_updates_atomically_with_remote_host",
        )
        .await;

        let mut remote = workspace("main", "project-a");
        remote.daemon_id = Some("daemon-abc".into());
        db.create_workspace(remote.clone()).await.unwrap();
        assert_eq!(
            db.get_workspace(remote.id.clone())
                .unwrap()
                .unwrap()
                .daemon_id,
            Some("daemon-abc".to_owned())
        );

        db.update_remote_host_and_daemon_id(
            remote.id.clone(),
            Some("dev-box".into()),
            Some("daemon-def".into()),
        )
        .await
        .unwrap();
        let stored = db.get_workspace(remote.id.clone()).unwrap().unwrap();
        assert_eq!(stored.remote_host.as_deref(), Some("dev-box"));
        assert_eq!(stored.daemon_id.as_deref(), Some("daemon-def"));
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

    #[gpui::test]
    async fn test_migration_dedup_keeps_the_winning_confirmed_row() {
        let name = "test_migration_dedup_keeps_the_winning_confirmed_row";
        let legacy = db::open_test_db::<BeforeConfirmedIdentityIndex>(name).await;

        // Branch metadata wins even when its row is older.
        let mut older = workspace("older-alias", "project-a");
        older.remote_host = Some("branch-box".into());
        older.terminal_session_id = Some("sess-1".into());
        older.branch = Some("feature/keep".into());
        older.last_opened_at = OffsetDateTime::from_unix_timestamp(1_000).unwrap();
        insert_confirmed_row(&legacy, older.clone(), Some(1)).await;

        let mut newer = workspace("newer-alias", "project-a");
        newer.remote_host = Some("branch-box".into());
        newer.terminal_session_id = Some("sess-1".into());
        newer.last_opened_at = OffsetDateTime::from_unix_timestamp(2_000).unwrap();
        insert_confirmed_row(&legacy, newer.clone(), Some(2)).await;

        // Legacy daemons use their host spelling as the effective identity.
        let mut legacy_older = workspace("legacy-older", "project-a");
        legacy_older.remote_host = Some("dev-box".into());
        legacy_older.terminal_session_id = Some("sess-2".into());
        legacy_older.last_opened_at = OffsetDateTime::from_unix_timestamp(1_000).unwrap();
        insert_confirmed_row(&legacy, legacy_older, Some(1)).await;

        let mut legacy_newer = workspace("legacy-newer", "project-a");
        legacy_newer.remote_host = Some("dev-box".into());
        legacy_newer.terminal_session_id = Some("sess-2".into());
        legacy_newer.last_opened_at = OffsetDateTime::from_unix_timestamp(2_000).unwrap();
        insert_confirmed_row(&legacy, legacy_newer.clone(), Some(2)).await;

        // Same daemon_id and session, same last_opened_at: the lexically
        // lower id wins the tie.
        let tied_at = OffsetDateTime::from_unix_timestamp(3_000).unwrap();
        let mut higher_id = workspace("tie-b", "project-a");
        higher_id.id = WorkspaceId::from("zzz-loser");
        higher_id.remote_host = Some("tie-box".into());
        higher_id.terminal_session_id = Some("sess-3".into());
        higher_id.last_opened_at = tied_at;
        insert_confirmed_row(&legacy, higher_id.clone(), Some(3)).await;

        let mut lower_id = workspace("tie-a", "project-a");
        lower_id.id = WorkspaceId::from("aaa-winner");
        lower_id.remote_host = Some("tie-box".into());
        lower_id.terminal_session_id = Some("sess-3".into());
        lower_id.last_opened_at = tied_at;
        insert_confirmed_row(&legacy, lower_id.clone(), Some(3)).await;

        let db = AdeWorkspaceRegistry::open_test_db(name).await;
        let listed = db.list_workspaces().unwrap();

        let branch_box: Vec<_> = listed
            .iter()
            .filter(|workspace| workspace.remote_host.as_deref() == Some("branch-box"))
            .collect();
        assert_eq!(branch_box.len(), 1, "only the winner survives: {listed:?}");
        assert_eq!(branch_box[0].id, older.id, "the row with a branch wins");
        assert_eq!(
            branch_box[0].name, older.name,
            "the winner's own metadata is kept, not merged with the loser's"
        );
        assert_eq!(branch_box[0].branch.as_deref(), Some("feature/keep"));

        let surviving_legacy = listed
            .iter()
            .find(|workspace| workspace.remote_host.as_deref() == Some("dev-box"))
            .expect("one legacy host row survives");
        assert_eq!(surviving_legacy.id, legacy_newer.id, "the newer row wins");

        let tie_box: Vec<_> = listed
            .iter()
            .filter(|workspace| workspace.remote_host.as_deref() == Some("tie-box"))
            .collect();
        assert_eq!(tie_box.len(), 1, "only the winner survives: {listed:?}");
        assert_eq!(
            tie_box[0].id, lower_id.id,
            "the lexically lower id wins the tie"
        );
    }

    #[gpui::test]
    async fn test_record_index_leaves_distinct_daemon_ids_separate() {
        let name = "test_record_index_leaves_distinct_daemon_ids_separate";
        let db = AdeWorkspaceRegistry::open_test_db(name).await;

        let mut first = workspace("first", "project-a");
        first.daemon_id = Some("daemon-a".into());
        first.terminal_session_id = Some("shared-session".into());
        db.create_workspace(first).await.unwrap();

        let mut second = workspace("second", "project-a");
        second.daemon_id = Some("daemon-b".into());
        second.terminal_session_id = Some("shared-session".into());
        db.create_workspace(second).await.unwrap();

        assert_eq!(
            db.list_workspaces().unwrap().len(),
            2,
            "different daemon_ids sharing a terminal_session_id are not duplicates"
        );
    }

    #[gpui::test]
    async fn test_migration_dedup_leaves_quarantined_rows_untouched() {
        let name = "test_migration_dedup_leaves_quarantined_rows_untouched";
        let legacy = db::open_test_db::<BeforeConfirmedIdentityIndex>(name).await;

        let mut first = workspace("first", "project-a");
        first.daemon_id = Some("daemon-x".into());
        first.terminal_session_id = Some("sess-1".into());
        insert_confirmed_row(&legacy, first.clone(), None).await;

        let mut second = workspace("second", "project-a");
        second.daemon_id = Some("daemon-x".into());
        second.terminal_session_id = Some("sess-1".into());
        insert_confirmed_row(&legacy, second.clone(), None).await;

        let db = AdeWorkspaceRegistry::open_test_db(name).await;
        assert_eq!(
            db.list_workspaces().unwrap().len(),
            2,
            "quarantined rows (used_by_client_at IS NULL) are never deleted or constrained"
        );
    }

    #[gpui::test]
    async fn test_create_workspace_preserves_existing_row_on_identity_conflict_but_replaces_by_id()
    {
        let db = AdeWorkspaceRegistry::open_test_db(
            "test_create_workspace_preserves_existing_row_on_identity_conflict",
        )
        .await;

        let mut original = workspace("original", "project-a");
        original.daemon_id = Some("daemon-x".into());
        original.terminal_session_id = Some("sess-1".into());
        db.create_workspace(original.clone()).await.unwrap();

        // A different id claiming the same confirmed identity must not
        // clobber the existing row or its id.
        let mut challenger = workspace("challenger", "project-a");
        challenger.daemon_id = Some("daemon-x".into());
        challenger.terminal_session_id = Some("sess-1".into());
        db.create_workspace(challenger.clone()).await.unwrap();

        assert_eq!(
            db.get_workspace(original.id.clone()).unwrap(),
            Some(original.clone()),
            "the existing row's id and metadata survive the index conflict"
        );
        assert!(
            db.get_workspace(challenger.id.clone()).unwrap().is_none(),
            "the challenger is never inserted"
        );
        assert_eq!(db.list_workspaces().unwrap().len(), 1);

        // Re-inserting under the *same* id still replaces it — the id
        // conflict clause, not the index clause, must win that case.
        let mut renamed = original.clone();
        renamed.name = "renamed".into();
        renamed.status = WorkspaceStatus::Running;
        db.create_workspace(renamed.clone()).await.unwrap();

        assert_eq!(
            db.get_workspace(original.id.clone()).unwrap(),
            Some(renamed),
            "a same-id insert still retains replace/update semantics"
        );
        assert_eq!(db.list_workspaces().unwrap().len(), 1);
    }
}
