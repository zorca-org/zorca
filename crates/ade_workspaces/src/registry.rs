use crate::{AdeWorkspace, WorkspaceId, WorkspaceStatus};
use anyhow::{Context as _, Result};
use db::{
    query,
    sqlez::{domain::Domain, statement::Statement, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use std::{cmp::Reverse, path::PathBuf};
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
        sql!(ALTER TABLE ade_workspaces ADD COLUMN project_identity TEXT;),
        sql!(
            ALTER TABLE ade_workspaces
            ADD COLUMN project_scope_rev INTEGER NOT NULL DEFAULT 0;
        ),
    ];
}

db::static_connection!(AdeWorkspaceRegistry, []);

/// Column order, matching the `Bind`/`Column` impls for [`AdeWorkspace`].
const COLUMNS: &str = "id,
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
    created_at,
    last_opened_at";

impl AdeWorkspaceRegistry {
    query! {
        fn workspace_history_table_count() -> Result<Vec<i64>> {
            SELECT COUNT(*) FROM sqlite_schema
            WHERE type = "table" AND name IN ("workspaces", "remote_connections")
        }
    }

    pub async fn backfill_project_identities_from_workspace_history(&self) -> Result<bool> {
        if self
            .workspace_history_table_count()?
            .first()
            .copied()
            .unwrap_or(0)
            != 2
        {
            return Ok(false);
        }

        self.write(|connection| {
            Statement::prepare(
                connection,
                "WITH candidate_identities AS (
                     SELECT ade.id, MIN(history.identity_paths) AS project_identity
                     FROM ade_workspaces ade
                     JOIN workspaces history
                       ON history.paths = CAST(ade.repository_path AS TEXT)
                     LEFT JOIN remote_connections remote
                       ON remote.id = history.remote_connection_id
                     WHERE ade.project_identity IS NULL
                       AND history.identity_paths IS NOT NULL
                       AND history.identity_paths <> ''
                       AND (
                         (ade.remote_host IS NULL AND history.remote_connection_id IS NULL)
                         OR (
                           ade.remote_host IS NOT NULL
                           AND remote.kind = 'ssh'
                           AND ade.remote_host = CASE
                             WHEN remote.port IS NOT NULL AND remote.port <> 22 THEN
                               'ssh://'
                               || CASE WHEN remote.user IS NULL THEN '' ELSE remote.user || '@' END
                               || CASE
                                    WHEN instr(remote.host, ':') > 0
                                      AND substr(remote.host, 1, 1) <> '['
                                    THEN '[' || remote.host || ']'
                                    ELSE remote.host
                                  END
                               || ':' || remote.port
                             ELSE
                               CASE WHEN remote.user IS NULL THEN '' ELSE remote.user || '@' END
                               || remote.host
                           END
                         )
                       )
                     GROUP BY ade.id
                     HAVING COUNT(DISTINCT history.identity_paths) = 1
                 )
                 UPDATE ade_workspaces
                 SET project_identity = (
                   SELECT candidate.project_identity
                   FROM candidate_identities candidate
                   WHERE candidate.id = ade_workspaces.id
                 )
                 WHERE id IN (SELECT id FROM candidate_identities)",
            )?
            .exec()
        })
        .await?;

        for workspace in self.list_workspaces()? {
            let Some(project_identity) = workspace.project_identity.as_deref() else {
                continue;
            };
            let project_id = crate::project_id_from_identity(project_identity);
            if workspace.project_id != project_id {
                let project_scope_rev = workspace
                    .project_scope_rev
                    .checked_add(1)
                    .context("project scope revision overflowed during history backfill")?;
                self.update_project_scope(
                    workspace.id,
                    project_scope_rev,
                    project_id,
                    project_identity.to_owned(),
                )
                .await?;
            }
        }
        Ok(true)
    }

    /// Replaces the row with the same id, but preserves the existing owner of a
    /// conflicting daemon record so callers can re-list it without data loss.
    pub async fn create_workspace(&self, workspace: AdeWorkspace) -> Result<()> {
        let query = format!(
            "INSERT INTO ade_workspaces ({COLUMNS}, used_by_client_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
            ON CONFLICT (id) DO UPDATE SET
                name = excluded.name,
                project_id = excluded.project_id,
                project_identity = excluded.project_identity,
                repository_path = excluded.repository_path,
                project_scope_rev = excluded.project_scope_rev,
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

    // Usage-only: a row is a workspace this client used, not one a daemon
    // happens to hold. Every normal read hides a quarantined row
    // (used_by_client_at IS NULL); only unconfirmed_workspaces() below sees it.
    query! {
        pub fn get_workspace(id: WorkspaceId) -> Result<Option<AdeWorkspace>> {
            SELECT
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
                created_at,
                last_opened_at
            FROM ade_workspaces
            WHERE id = ? AND used_by_client_at IS NOT NULL
        }
    }

    query! {
        pub fn list_workspaces() -> Result<Vec<AdeWorkspace>> {
            SELECT
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
                created_at,
                last_opened_at
            FROM ade_workspaces
            WHERE used_by_client_at IS NOT NULL
            ORDER BY last_opened_at DESC
        }
    }

    query! {
        pub fn list_workspaces_for_project(project_id: String) -> Result<Vec<AdeWorkspace>> {
            SELECT
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
                created_at,
                last_opened_at
            FROM ade_workspaces
            WHERE project_id = ? AND used_by_client_at IS NOT NULL
            ORDER BY last_opened_at DESC
        }
    }

    /// Quarantined rows in deterministic promotion order: current daemon,
    /// branch metadata, recency, then id.
    pub(crate) fn promotion_candidates(
        &self,
        daemon_id: Option<&str>,
    ) -> Result<Vec<AdeWorkspace>> {
        let mut rows = self.unconfirmed_workspaces()?;
        rows.sort_by(|left, right| {
            let key = |row: &AdeWorkspace| {
                (
                    !daemon_id.is_some_and(|id| row.daemon_id.as_deref() == Some(id)),
                    row.branch.is_none(),
                    Reverse(row.last_opened_at),
                )
            };
            key(left)
                .cmp(&key(right))
                .then_with(|| left.id.as_str().cmp(right.id.as_str()))
        });
        Ok(rows)
    }

    query! {
        pub(crate) fn unconfirmed_workspaces() -> Result<Vec<AdeWorkspace>> {
            SELECT
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
                created_at,
                last_opened_at
            FROM ade_workspaces
            WHERE used_by_client_at IS NULL
        }
    }

    /// Promotes a quarantined row if nobody has claimed it first.
    pub(crate) async fn confirm_workspace(
        &self,
        id: WorkspaceId,
        remote_host: Option<String>,
        daemon_id: Option<String>,
        at: OffsetDateTime,
    ) -> Result<bool> {
        Ok(self
            .confirm_workspace_internal(id, remote_host, daemon_id, at.unix_timestamp())
            .await?
            .is_some())
    }

    query! {
        async fn confirm_workspace_internal(
            id: WorkspaceId,
            remote_host: Option<String>,
            daemon_id: Option<String>,
            at: i64
        ) -> Result<Option<WorkspaceId>> {
            UPDATE ade_workspaces
            SET remote_host = ?2, daemon_id = ?3, used_by_client_at = ?4, last_opened_at = ?4
            WHERE id = ?1 AND used_by_client_at IS NULL
            RETURNING id
        }
    }

    query! {
        pub async fn update_status(id: WorkspaceId, status: WorkspaceStatus) -> Result<()> {
            UPDATE ade_workspaces SET status = ?2 WHERE id = ?1
        }
    }

    /// Rebinds route and daemon identity only if the row still has the values
    /// the caller read.
    pub(crate) async fn rebind_workspace_route(
        &self,
        id: WorkspaceId,
        expected_remote_host: Option<String>,
        expected_daemon_id: Option<String>,
        remote_host: Option<String>,
        daemon_id: Option<String>,
    ) -> Result<bool> {
        Ok(self
            .rebind_workspace_route_internal(
                id,
                expected_remote_host,
                expected_daemon_id,
                remote_host,
                daemon_id,
            )
            .await?
            .is_some())
    }

    query! {
        async fn rebind_workspace_route_internal(
            id: WorkspaceId,
            expected_remote_host: Option<String>,
            expected_daemon_id: Option<String>,
            remote_host: Option<String>,
            daemon_id: Option<String>
        ) -> Result<Option<WorkspaceId>> {
            UPDATE ade_workspaces
            SET remote_host = ?4, daemon_id = ?5
            WHERE id = ?1 AND remote_host IS ?2 AND daemon_id IS ?3
            RETURNING id
        }
    }

    /// Resolves a route rebind that collided with another row already bound to
    /// the same daemon record. The migration's survivor order decides which
    /// metadata survives; the verified loser is deleted in the same savepoint.
    pub(crate) async fn resolve_rebind_conflict(
        &self,
        candidate_id: WorkspaceId,
        expected_remote_host: Option<String>,
        expected_daemon_id: Option<String>,
        remote_host: Option<String>,
        daemon_id: Option<String>,
        terminal_session_id: String,
    ) -> Result<Option<WorkspaceId>> {
        if daemon_id.is_none() {
            return Ok(None);
        }
        self.write(move |connection| {
            connection.with_savepoint_rollback("ade_resolve_rebind", || {
                // Acquire the write lock before the read that chooses a
                // survivor, so another process cannot change either row in
                // between.
                Statement::prepare(connection, "UPDATE ade_workspaces SET id = id WHERE 0")?
                    .exec()?;

                let query = format!(
                    "SELECT {COLUMNS}
                     FROM ade_workspaces
                     WHERE used_by_client_at IS NOT NULL
                       AND (id = ?1 OR (daemon_id IS ?2 AND terminal_session_id = ?3))"
                );
                let mut statement = Statement::prepare(connection, query)?;
                let next_index = statement.bind(&candidate_id, 1)?;
                let next_index = statement.bind(&daemon_id, next_index)?;
                statement.bind(&terminal_session_id, next_index)?;
                let rows = statement.rows::<AdeWorkspace>()?;

                let owner = rows.iter().find(|row| {
                    row.daemon_id == daemon_id
                        && row.terminal_session_id.as_deref() == Some(&terminal_session_id)
                });
                let Some(owner) = owner else {
                    return Ok(None);
                };
                let candidate = rows.iter().find(|row| {
                    row.id == candidate_id
                        && row.remote_host == expected_remote_host
                        && row.daemon_id == expected_daemon_id
                        && row.terminal_session_id.as_deref() == Some(&terminal_session_id)
                });

                let mut contenders = vec![owner];
                if let Some(candidate) = candidate
                    && candidate.id != owner.id
                {
                    contenders.push(candidate);
                }
                contenders.sort_by(|left, right| {
                    (left.branch.is_none(), Reverse(left.last_opened_at))
                        .cmp(&(right.branch.is_none(), Reverse(right.last_opened_at)))
                        .then_with(|| left.id.as_str().cmp(right.id.as_str()))
                });
                let survivor = *contenders
                    .first()
                    .context("a rebind conflict has no survivor")?;

                for loser in contenders.into_iter().skip(1) {
                    let mut delete =
                        Statement::prepare(connection, "DELETE FROM ade_workspaces WHERE id = ?1")?;
                    delete.bind(&loser.id, 1)?;
                    delete.exec()?;
                }

                let mut update = Statement::prepare(
                    connection,
                    "UPDATE ade_workspaces
                     SET remote_host = ?2, daemon_id = ?3
                     WHERE id = ?1",
                )?;
                let next_index = update.bind(&survivor.id, 1)?;
                let next_index = update.bind(&remote_host, next_index)?;
                update.bind(&daemon_id, next_index)?;
                update.exec()?;
                Ok(Some(survivor.id.clone()))
            })
        })
        .await
    }

    // Renames a workspace. The id is untouched — a name is display metadata,
    // and everything else (the session, the daemon's workspace record, this
    // row) is keyed by the id. See `AdeWorkspace::daemon_workspace_id`.
    query! {
        pub async fn update_name(id: WorkspaceId, name: String) -> Result<()> {
            UPDATE ade_workspaces SET name = ?2 WHERE id = ?1
        }
    }

    pub async fn update_project_scope(
        &self,
        id: WorkspaceId,
        project_scope_rev: u64,
        project_id: String,
        project_identity: String,
    ) -> Result<(bool, AdeWorkspace)> {
        let applied = self
            .update_project_scope_if_newer(
                id.clone(),
                project_scope_rev,
                project_id,
                project_identity,
            )
            .await?
            .is_some();
        let stored = self.get_workspace(id.clone())?.with_context(|| {
            format!("workspace {id} disappeared while updating its project scope")
        })?;
        Ok((applied, stored))
    }

    query! {
        async fn update_project_scope_if_newer(
            id: WorkspaceId,
            project_scope_rev: u64,
            project_id: String,
            project_identity: String
        ) -> Result<Option<WorkspaceId>> {
            UPDATE ade_workspaces
            SET project_scope_rev = ?2, project_id = ?3, project_identity = ?4
            WHERE id = ?1
              AND (project_scope_rev < ?2
                   OR (project_scope_rev = ?2 AND project_identity IS NULL))
            RETURNING id
        }
    }

    pub async fn update_repository_scope(
        &self,
        id: WorkspaceId,
        repository_path: PathBuf,
        project_scope_rev: u64,
        project_id: String,
        project_identity: String,
    ) -> Result<(bool, AdeWorkspace)> {
        let applied = self
            .update_repository_scope_if_newer(
                id.clone(),
                repository_path,
                project_scope_rev,
                project_id,
                project_identity,
            )
            .await?
            .is_some();
        let stored = self.get_workspace(id.clone())?.with_context(|| {
            format!("workspace {id} disappeared while updating its repository scope")
        })?;
        Ok((applied, stored))
    }

    query! {
        async fn update_repository_scope_if_newer(
            id: WorkspaceId,
            repository_path: PathBuf,
            project_scope_rev: u64,
            project_id: String,
            project_identity: String
        ) -> Result<Option<WorkspaceId>> {
            UPDATE ade_workspaces
            SET repository_path = ?2,
                project_scope_rev = ?3,
                project_id = ?4,
                project_identity = ?5
            WHERE id = ?1
              AND (project_scope_rev < ?3
                   OR (project_scope_rev = ?3 AND project_identity IS NULL))
            RETURNING id
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
    // Puts a row back in the quarantine the usage migration leaves legacy rows
    // in, so a promotion can be tested against a real one.
    query! {
        pub(crate) async fn quarantine_workspace(id: WorkspaceId) -> Result<()> {
            UPDATE ade_workspaces SET used_by_client_at = NULL WHERE id = ?
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

    const BEFORE_PROJECT_IDENTITY_COLUMNS: &str = "id,
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
            "INSERT INTO ade_workspaces ({BEFORE_PROJECT_IDENTITY_COLUMNS}, used_by_client_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
        );
        connection
            .write(move |connection| {
                let mut statement = Statement::prepare(connection, &query)?;
                let next_index = statement.bind(&workspace.id, 1)?;
                let next_index = statement.bind(&workspace.name, next_index)?;
                let next_index = statement.bind(&workspace.project_id, next_index)?;
                let next_index = statement.bind(&workspace.repository_path, next_index)?;
                let next_index = statement.bind(&workspace.branch, next_index)?;
                let next_index = statement.bind(&workspace.remote_host, next_index)?;
                let next_index = statement.bind(&workspace.remote_workspace_path, next_index)?;
                let next_index = statement.bind(&workspace.terminal_session_id, next_index)?;
                let next_index = statement.bind(&workspace.daemon_id, next_index)?;
                let next_index = statement.bind(&workspace.status, next_index)?;
                let next_index =
                    statement.bind(&workspace.created_at.unix_timestamp(), next_index)?;
                let next_index =
                    statement.bind(&workspace.last_opened_at.unix_timestamp(), next_index)?;
                statement.bind(&used_by_client_at, next_index)?;
                statement.exec()
            })
            .await
            .unwrap();
    }

    #[gpui::test]
    async fn test_migration_quarantines_rows_written_before_the_usage_column() {
        let name = "test_usage_provenance_rollback";
        let legacy = db::open_test_db::<BeforeUsageProvenance>(name).await;
        let before_upgrade = workspace("before-upgrade", "project-a");
        insert_without_usage_column(&legacy, before_upgrade.clone()).await;

        // A row from before the usage column existed is quarantined, not
        // smuggled past the reads below, whichever migration wrote it.
        let db = AdeWorkspaceRegistry::open_test_db(name).await;
        assert!(
            db.get_workspace(before_upgrade.id.clone())
                .unwrap()
                .is_none()
        );
        assert_eq!(db.unconfirmed_workspaces().unwrap().len(), 1);

        let after_rollback = workspace("after-rollback", "project-a");
        insert_without_usage_column(&legacy, after_rollback.clone()).await;
        drop(db);

        // A downgrade-and-back writes the same way and is quarantined the
        // same way: NULL is the default an older build's insert takes.
        let db = AdeWorkspaceRegistry::open_test_db(name).await;
        assert!(
            db.get_workspace(after_rollback.id.clone())
                .unwrap()
                .is_none()
        );
        assert_eq!(db.unconfirmed_workspaces().unwrap().len(), 2);

        let created_here = workspace("created-here", "project-a");
        db.create_workspace(created_here.clone()).await.unwrap();
        assert_eq!(
            db.get_workspace(created_here.id.clone()).unwrap(),
            Some(created_here)
        );
    }

    #[gpui::test]
    async fn test_workspace_round_trip() {
        let db = AdeWorkspaceRegistry::open_test_db("test_workspace_round_trip").await;

        let mut local = workspace("main", "project-a");
        local.project_identity = Some("/repos/project-a".into());
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

        db.update_project_scope(remote.id.clone(), 1, "zed".into(), "/repos/zed".into())
            .await
            .unwrap();
        let stored = db.get_workspace(remote.id.clone()).unwrap().unwrap();
        assert_eq!(stored.project_id, "zed");
        assert_eq!(stored.project_identity.as_deref(), Some("/repos/zed"));
        assert_eq!(stored.project_scope_rev, 1);
        let (applied, winner) = db
            .update_repository_scope(
                remote.id.clone(),
                PathBuf::from("/repos/stale"),
                1,
                "stale".into(),
                "/repos/stale".into(),
            )
            .await
            .unwrap();
        assert!(!applied, "an equal-revision mismatch must lose");
        assert_eq!(winner, stored);
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
    async fn test_project_identity_history_backfill_respects_workspace_location() {
        let absent = AdeWorkspaceRegistry::open_test_db("test_identity_backfill_absent").await;
        assert!(
            !absent
                .backfill_project_identities_from_workspace_history()
                .await
                .unwrap(),
            "an isolated registry has no workspace history tables"
        );

        let db = AdeWorkspaceRegistry::open_test_db("test_identity_backfill_by_location").await;
        db.write(|connection| {
            Statement::prepare(
                connection,
                "CREATE TABLE remote_connections(
                     id INTEGER PRIMARY KEY,
                     kind TEXT NOT NULL,
                     host TEXT,
                     port INTEGER,
                     user TEXT
                 )",
            )?
            .exec()?;
            Statement::prepare(
                connection,
                "CREATE TABLE workspaces(
                     paths TEXT,
                     identity_paths TEXT,
                     remote_connection_id INTEGER
                 )",
            )?
            .exec()?;
            Statement::prepare(
                connection,
                "INSERT INTO remote_connections(id, kind, host, port, user) VALUES
                   (1, 'ssh', 'host-a', NULL, 'user'),
                   (2, 'ssh', 'host-b', NULL, 'user'),
                   (3, 'ssh', 'host-c', 2222, 'alice'),
                   (4, 'docker', 'host-d', NULL, 'user')",
            )?
            .exec()?;
            Statement::prepare(
                connection,
                "INSERT INTO workspaces(paths, identity_paths, remote_connection_id) VALUES
                   ('/checkout/shared', '/repos/local-project', NULL),
                   ('/checkout/shared', '/repos/a-project', 1),
                   ('/checkout/shared', '/repos/b-project', 2),
                   ('/checkout/port', '/repos/c-project', 3),
                   ('/checkout/docker', '/repos/docker-project', 4),
                   ('/checkout/ambiguous', '/repos/a-project', 1),
                   ('/checkout/ambiguous', '/repos/other-project', 1)",
            )?
            .exec()
        })
        .await
        .unwrap();

        let local = AdeWorkspace::new("local", "shared", "/checkout/shared");
        let mut host_a = AdeWorkspace::new("host-a", "shared", "/checkout/shared");
        host_a.remote_host = Some("user@host-a".to_owned());
        let mut host_b = AdeWorkspace::new("host-b", "shared", "/checkout/shared");
        host_b.remote_host = Some("user@host-b".to_owned());
        let mut nondefault_port = AdeWorkspace::new("host-c", "port", "/checkout/port");
        nondefault_port.remote_host = Some("ssh://alice@host-c:2222".to_owned());
        let mut alias = AdeWorkspace::new("alias", "shared", "/checkout/shared");
        alias.remote_host = Some("host-a".to_owned());
        let mut non_ssh = AdeWorkspace::new("docker", "docker", "/checkout/docker");
        non_ssh.remote_host = Some("user@host-d".to_owned());
        let mut ambiguous = AdeWorkspace::new("ambiguous", "ambiguous", "/checkout/ambiguous");
        ambiguous.remote_host = Some("user@host-a".to_owned());

        for workspace in [
            &local,
            &host_a,
            &host_b,
            &nondefault_port,
            &alias,
            &non_ssh,
            &ambiguous,
        ] {
            db.create_workspace(workspace.clone()).await.unwrap();
        }

        assert!(
            db.backfill_project_identities_from_workspace_history()
                .await
                .unwrap()
        );
        for (workspace, expected_identity, expected_label) in [
            (&local, "/repos/local-project", "local-project"),
            (&host_a, "/repos/a-project", "a-project"),
            (&host_b, "/repos/b-project", "b-project"),
            (&nondefault_port, "/repos/c-project", "c-project"),
        ] {
            let stored = db.get_workspace(workspace.id.clone()).unwrap().unwrap();
            assert_eq!(
                stored.project_identity.as_deref(),
                Some(expected_identity),
                "{} must use history from its own location",
                workspace.name
            );
            assert_eq!(stored.project_id, expected_label);
        }
        for workspace in [&alias, &non_ssh, &ambiguous] {
            assert!(
                db.get_workspace(workspace.id.clone())
                    .unwrap()
                    .unwrap()
                    .project_identity
                    .is_none(),
                "{} has no unambiguous exact SSH location",
                workspace.name
            );
        }
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
            db.unconfirmed_workspaces().unwrap().len(),
            2,
            "quarantined rows (used_by_client_at IS NULL) are never deleted or constrained"
        );
    }

    #[gpui::test]
    async fn test_promotion_candidates_have_a_total_survivor_order() {
        let db = AdeWorkspaceRegistry::open_test_db("test_promotion_candidate_order").await;
        let candidate = |id: &str,
                         daemon_id: Option<&str>,
                         branch: Option<&str>,
                         last_opened_at: i64| AdeWorkspace {
            id: WorkspaceId::from(id),
            daemon_id: daemon_id.map(str::to_owned),
            branch: branch.map(str::to_owned),
            last_opened_at: OffsetDateTime::from_unix_timestamp(last_opened_at).unwrap(),
            ..workspace(id, "project-a")
        };
        for row in [
            candidate("exact", Some("daemon-a"), None, 100),
            candidate("branch", None, Some("feature/x"), 100),
            candidate("newer", None, None, 300),
            candidate("tie-a", None, None, 200),
            candidate("tie-b", None, None, 200),
        ] {
            insert_confirmed_row(&db, row, None).await;
        }

        let ids = |rows: Vec<AdeWorkspace>| rows.into_iter().map(|row| row.id).collect::<Vec<_>>();
        assert_eq!(
            ids(db.promotion_candidates(Some("daemon-a")).unwrap()),
            ["exact", "branch", "newer", "tie-a", "tie-b"]
                .map(WorkspaceId::from)
                .to_vec()
        );
        assert_eq!(
            ids(db.promotion_candidates(None).unwrap()),
            ["branch", "newer", "tie-a", "tie-b", "exact"]
                .map(WorkspaceId::from)
                .to_vec()
        );
    }

    #[gpui::test]
    async fn test_confirming_a_quarantined_row_is_a_single_winner_claim() {
        let db = AdeWorkspaceRegistry::open_test_db("test_confirm_workspace_cas").await;
        let mut row = workspace("contested", "project-a");
        row.terminal_session_id = Some("sess-1".into());
        insert_confirmed_row(&db, row.clone(), None).await;

        let won_at = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        assert!(
            db.confirm_workspace(
                row.id.clone(),
                Some("box-a".into()),
                Some("daemon-a".into()),
                won_at,
            )
            .await
            .unwrap()
        );
        assert!(
            !db.confirm_workspace(
                row.id.clone(),
                Some("box-b".into()),
                Some("daemon-b".into()),
                OffsetDateTime::from_unix_timestamp(1_900_000_000).unwrap(),
            )
            .await
            .unwrap()
        );

        let stored = db.get_workspace(row.id).unwrap().unwrap();
        assert_eq!(stored.remote_host.as_deref(), Some("box-a"));
        assert_eq!(stored.daemon_id.as_deref(), Some("daemon-a"));
        assert_eq!(stored.last_opened_at, won_at);
    }

    #[gpui::test]
    async fn test_rebinding_a_route_is_a_single_winner_write() {
        let db = AdeWorkspaceRegistry::open_test_db("test_rebind_workspace_route_cas").await;
        let mut row = workspace("legacy", "project-a");
        row.remote_host = Some("old-alias".into());
        db.create_workspace(row.clone()).await.unwrap();

        assert!(
            db.rebind_workspace_route(
                row.id.clone(),
                Some("old-alias".into()),
                None,
                Some("new-alias".into()),
                Some("daemon-a".into()),
            )
            .await
            .unwrap()
        );
        assert!(
            !db.rebind_workspace_route(
                row.id.clone(),
                Some("old-alias".into()),
                None,
                Some("other-alias".into()),
                Some("daemon-b".into()),
            )
            .await
            .unwrap()
        );

        let stored = db.get_workspace(row.id).unwrap().unwrap();
        assert_eq!(stored.remote_host.as_deref(), Some("new-alias"));
        assert_eq!(stored.daemon_id.as_deref(), Some("daemon-a"));
    }

    #[gpui::test]
    async fn test_rebind_conflict_keeps_the_deterministic_legacy_survivor() {
        let db = AdeWorkspaceRegistry::open_test_db("test_rebind_conflict_survivor").await;
        let session_id = "ade-main-000001";

        let mut first = workspace("first", "project-a");
        first.remote_host = Some("alias-a".into());
        first.terminal_session_id = Some(session_id.into());
        first.last_opened_at = OffsetDateTime::from_unix_timestamp(200).unwrap();
        db.create_workspace(first.clone()).await.unwrap();

        let mut preferred = workspace("preferred", "project-a");
        preferred.remote_host = Some("alias-b".into());
        preferred.terminal_session_id = Some(session_id.into());
        preferred.branch = Some("feature/x".into());
        preferred.last_opened_at = OffsetDateTime::from_unix_timestamp(100).unwrap();
        db.create_workspace(preferred.clone()).await.unwrap();

        assert!(
            db.rebind_workspace_route(
                first.id.clone(),
                first.remote_host.clone(),
                None,
                Some("alias-a".into()),
                Some("daemon-a".into()),
            )
            .await
            .unwrap()
        );
        assert!(
            db.rebind_workspace_route(
                preferred.id.clone(),
                preferred.remote_host.clone(),
                None,
                Some("alias-b".into()),
                Some("daemon-a".into()),
            )
            .await
            .is_err(),
            "the second legacy row collides with the owner bound first"
        );

        let survivor = db
            .resolve_rebind_conflict(
                preferred.id.clone(),
                preferred.remote_host.clone(),
                None,
                Some("alias-b".into()),
                Some("daemon-a".into()),
                session_id.into(),
            )
            .await
            .unwrap();
        assert_eq!(survivor, Some(preferred.id.clone()));

        let rows = db.list_workspaces().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, preferred.id);
        assert_eq!(rows[0].branch.as_deref(), Some("feature/x"));
        assert_eq!(rows[0].remote_host.as_deref(), Some("alias-b"));
        assert_eq!(rows[0].daemon_id.as_deref(), Some("daemon-a"));
    }

    #[gpui::test]
    async fn test_confirming_a_quarantined_row_makes_it_visible_and_preserves_metadata() {
        let db = AdeWorkspaceRegistry::open_test_db(
            "test_confirming_a_quarantined_row_makes_it_visible_and_preserves_metadata",
        )
        .await;

        let mut quarantined = workspace("legacy", "project-a");
        quarantined.branch = Some("feature/x".into());
        quarantined.remote_host = Some("dev-box".into());
        quarantined.terminal_session_id = Some("sess-1".into());
        insert_confirmed_row(&db, quarantined.clone(), None).await;

        // Quarantined: hidden from every normal read.
        assert!(db.get_workspace(quarantined.id.clone()).unwrap().is_none());
        assert!(db.list_workspaces().unwrap().is_empty());
        assert!(
            db.list_workspaces_for_project("project-a".into())
                .unwrap()
                .is_empty()
        );
        assert_eq!(db.unconfirmed_workspaces().unwrap().len(), 1);

        let confirmed_at = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        db.confirm_workspace(
            quarantined.id.clone(),
            quarantined.remote_host.clone(),
            quarantined.daemon_id.clone(),
            confirmed_at,
        )
        .await
        .unwrap();

        // Confirmed: visible everywhere, id and metadata untouched, timestamp updated.
        let confirmed = db.get_workspace(quarantined.id.clone()).unwrap().unwrap();
        assert_eq!(confirmed.id, quarantined.id);
        assert_eq!(confirmed.branch.as_deref(), Some("feature/x"));
        assert_eq!(confirmed.remote_host.as_deref(), Some("dev-box"));
        assert_eq!(confirmed.terminal_session_id.as_deref(), Some("sess-1"));
        assert_eq!(confirmed.last_opened_at, confirmed_at);
        assert_eq!(db.list_workspaces().unwrap().len(), 1);
        assert!(db.unconfirmed_workspaces().unwrap().is_empty());
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
