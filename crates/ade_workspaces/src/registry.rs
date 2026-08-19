use crate::{AdeWorkspace, WorkspaceId, WorkspaceStatus, now_whole_seconds};
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
///
/// **Usage-only.** A row is a workspace *this client used*, not one some daemon
/// happens to hold: every read below hides a row whose `used_by_client_at` is
/// NULL, and only creating or opening one writes that column. A daemon listing
/// is discovery, and discovery lives in memory — see [`crate::Reconciled`].
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
        // Provenance rather than a purge: every pre-existing row migrates to
        // NULL — quarantined, not deleted, because the daemon can re-derive a
        // name and a root but not a branch, a local uuid, or history. NULL is
        // also the default an *older* build's insert takes, so a downgrade and
        // back cannot smuggle adopted rows past the reads below.
        sql!(ALTER TABLE ade_workspaces ADD COLUMN used_by_client_at INTEGER;),
        // Which daemon holds the record, as distinct from how its host was
        // spelled — see [`AdeWorkspace::daemon_id`]. NULL for every row
        // written before a daemon reported one, and for hosts whose daemon is
        // too old to; those rows are identified by their spelling, which is
        // what the index below falls back to.
        sql!(ALTER TABLE ade_workspaces ADD COLUMN daemon_id TEXT;),
        // Collapse before constraining, or the index cannot be created at all.
        // The survivor is the one `WorkspaceLifecycleService::collapse_duplicate_rows`
        // would pick — metadata a mirror cannot re-derive, then recency, then
        // the lower uuid — so a duplicate dies the same death whichever of the
        // two paths finds it first. Spelling-scoped, because a row written
        // before this migration records no daemon and the client is not
        // running yet to ask one.
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
        // **One confirmed row per daemon record.** Two spellings of one host
        // used to mint a row each, under separate locks that never saw each
        // other; this is the fence that holds when the locks do not.
        //
        // Partial and by expression, because both halves of the key are
        // nullable and neither NULL means "any": a quarantined row is not a
        // use, a row with no wire id addresses no record, and a local host is
        // one host rather than a wildcard. `COALESCE` is what makes the
        // spelling the identity of a row whose daemon never named itself, and
        // the `char(0)` prefix keeps that fallback in a namespace of its own —
        // a daemon id is a uuid and can never begin with a NUL, so no host
        // spelling can be mistaken for one.
        sql!(
            CREATE UNIQUE INDEX IF NOT EXISTS ade_workspaces_one_row_per_record
            ON ade_workspaces (
                COALESCE(daemon_id, char(0) || COALESCE(remote_host, char(0))),
                terminal_session_id
            )
            WHERE used_by_client_at IS NOT NULL AND terminal_session_id IS NOT NULL;
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
    /// Inserts a workspace **this client is using**, replacing any row with the
    /// same id.
    ///
    /// Every caller is a create or an open, so the row is confirmed here rather
    /// than by a second call somebody could forget.
    pub async fn create_workspace(&self, workspace: AdeWorkspace) -> Result<()> {
        self.insert_workspace(workspace, Some(now_whole_seconds().unix_timestamp()))
            .await
    }

    /// Makes every later read and write fail, the way a database that has gone
    /// away does. The only way a test reaches the paths that compensate for a
    /// registry this client could not write.
    #[cfg(test)]
    pub(crate) async fn break_for_test(&self) {
        self.write(|connection| connection.exec("DROP TABLE ade_workspaces")?())
            .await
            .expect("dropping the table");
    }

    /// A row as the migration leaves one: present, and invisible to every read
    /// until something confirms it. Nothing in production writes one — only an
    /// older build, and the migration itself.
    #[cfg(test)]
    pub(crate) async fn create_unconfirmed_workspace(&self, workspace: AdeWorkspace) -> Result<()> {
        self.insert_workspace(workspace, None).await
    }

    async fn insert_workspace(
        &self,
        workspace: AdeWorkspace,
        used_by_client_at: Option<i64>,
    ) -> Result<()> {
        let query = format!(
            "INSERT OR REPLACE INTO ade_workspaces ({COLUMNS}, used_by_client_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
        );
        self.write(move |connection| {
            let mut statement = Statement::prepare(connection, &query)?;
            let next_index = statement.bind(&workspace, 1)?;
            statement.bind(&used_by_client_at, next_index)?;
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
            WHERE id = ? AND used_by_client_at IS NOT NULL
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
            WHERE project_id = ? AND used_by_client_at IS NOT NULL
            ORDER BY last_opened_at DESC
        }
    }

    // The quarantine: rows no read above will ever return. The one caller is
    // promotion — see `WorkspaceLifecycleService::persist_on_open` — which
    // matches them by host, wire id and root before confirming one.
    query! {
        pub(crate) fn unconfirmed_workspaces() -> Result<Vec<AdeWorkspace>> {
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
            WHERE used_by_client_at IS NULL
        }
    }

    /// Promotes a quarantined row in place: it keeps its uuid, its branch and
    /// its remote metadata — the things a daemon record cannot re-derive — and
    /// counts as opened now.
    pub async fn confirm_workspace(&self, id: WorkspaceId, at: OffsetDateTime) -> Result<()> {
        self.confirm_workspace_internal(id, at.unix_timestamp())
            .await
    }

    query! {
        async fn confirm_workspace_internal(id: WorkspaceId, at: i64) -> Result<()> {
            UPDATE ade_workspaces SET used_by_client_at = ?2, last_opened_at = ?2 WHERE id = ?1
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

    // Drops whatever row addresses one daemon record — the removal path, which
    // knows the host and the wire id and not this client's uuid.
    //
    // `IS` rather than `=`, because a local workspace's host is NULL and `=`
    // would match nothing. Daemon-scoped on purpose: two *daemons* may hold the
    // same id, so the removal reaches the spelling it was announced under plus
    // every other spelling of the same daemon — and, when the daemon named
    // itself, nothing else. A NULL `daemon_id` argument matches by spelling
    // alone; it must never match rows whose column is NULL, which is every row
    // of every other host. Quarantined rows go too — the record they name is
    // gone. Matching nothing is a success: a removal is announced to every
    // client, and only one of them has the row.
    query! {
        pub async fn delete_workspaces_for_record(
            remote_host: Option<String>,
            daemon_id: Option<String>,
            wire_id: String
        ) -> Result<()> {
            DELETE FROM ade_workspaces
            WHERE terminal_session_id = ?3
              AND (remote_host IS ?1 OR (?2 IS NOT NULL AND daemon_id IS ?2))
        }
    }

    // Records which daemon a row's host spelling turned out to name — see
    // [`AdeWorkspace::daemon_id`]. Written when a row is created against a
    // daemon that named itself, and when a pass finds one that predates the
    // field.
    query! {
        pub async fn update_daemon_id(id: WorkspaceId, daemon_id: Option<String>) -> Result<()> {
            UPDATE ade_workspaces SET daemon_id = ?2 WHERE id = ?1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The schema as it stood before the one-row-per-record index: the same
    /// domain, so the migrations table lines up and reopening under
    /// [`AdeWorkspaceRegistry`] runs only what is missing.
    struct BeforeTheRecordIndex;

    impl Domain for BeforeTheRecordIndex {
        const NAME: &str = <AdeWorkspaceRegistry as Domain>::NAME;
        const MIGRATIONS: &[&str] = &[
            <AdeWorkspaceRegistry as Domain>::MIGRATIONS[0],
            <AdeWorkspaceRegistry as Domain>::MIGRATIONS[1],
        ];
    }

    /// **Collapse, then constrain.** A build before the index could leave two
    /// confirmed rows for one record, and adding a unique index over them would
    /// fail outright — so the migration drops the loser first, by the same rule
    /// a reconcile pass uses: metadata a mirror cannot re-derive, then recency,
    /// then the lower uuid.
    #[gpui::test]
    async fn test_the_migration_collapses_duplicates_before_constraining() {
        // Held open for the whole test: the test db is in memory and lives
        // exactly as long as a connection to it does.
        let legacy =
            db::open_test_db::<BeforeTheRecordIndex>("test_registry_collapse_migration").await;
        let row = |id: &str, branch: &str, wire: &str, host: &str, opened: i64| {
            format!(
                "INSERT INTO ade_workspaces (id, name, project_id, repository_path, branch, \
                 remote_host, remote_workspace_path, terminal_session_id, status, created_at, \
                 last_opened_at, used_by_client_at) VALUES ({id}, 'zed', 'zed', \
                 CAST('/repos/zed' AS BLOB), {branch}, {host}, NULL, {wire}, 'running', 1, \
                 {opened}, 1)",
                id = quoted(id),
                branch = quoted_or_null(branch),
                host = quoted_or_null(host),
                wire = quoted(wire),
            )
        };
        for statement in [
            // The pair: the newer one has no branch, so recency alone would
            // keep the wrong row.
            row("row-mine", "feature/x", "ws-1", "", 100),
            row("row-theirs", "", "ws-1", "", 200),
            // Same id on another host, and a row of its own: neither is a
            // duplicate of anything.
            row("row-elsewhere", "", "ws-1", "dev-box", 300),
            row("row-other", "", "ws-2", "", 400),
        ] {
            legacy
                .write(move |connection| connection.exec(&statement)?())
                .await
                .expect("seeding a pre-index row");
        }

        let db = AdeWorkspaceRegistry::open_test_db("test_registry_collapse_migration").await;
        let mut surviving: Vec<String> = db
            .list_workspaces()
            .unwrap()
            .into_iter()
            .map(|workspace| workspace.id.to_string())
            .collect();
        surviving.sort();
        assert_eq!(
            surviving,
            vec![
                "row-elsewhere".to_owned(),
                "row-mine".to_owned(),
                "row-other".to_owned()
            ],
            "only the loser of the pair goes"
        );

        // And the index holds from here on: a second confirmed row for a
        // record is not a second row. Nothing in the lifecycle layer reaches
        // this — `persist_on_open` answers with the row it finds — which is
        // what makes replacement the fence rather than the normal path.
        let mut second = workspace("zed", "zed");
        second.terminal_session_id = Some("ws-2".to_owned());
        db.create_workspace(second.clone()).await.unwrap();
        let ids: Vec<String> = db
            .list_workspaces()
            .unwrap()
            .into_iter()
            .map(|workspace| workspace.id.to_string())
            .collect();
        assert_eq!(ids.len(), 3, "{ids:?}");
        assert!(ids.contains(&second.id.to_string()));
        assert!(!ids.contains(&"row-other".to_owned()));
    }

    fn quoted(value: &str) -> String {
        format!("'{value}'")
    }

    fn quoted_or_null(value: &str) -> String {
        if value.is_empty() {
            "NULL".to_owned()
        } else {
            quoted(value)
        }
    }

    fn workspace(name: &str, project_id: &str) -> AdeWorkspace {
        AdeWorkspace::new(name, project_id, "/repos/zed")
    }

    #[gpui::test]
    async fn test_workspace_round_trip() {
        let db = AdeWorkspaceRegistry::open_test_db("test_workspace_round_trip").await;

        let mut local = workspace("main", "project-a");
        local.branch = Some("main".into());
        local.terminal_session_id = Some("ws-local-1".to_owned());
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

    /// A row the migration quarantined is present and unreadable: no listing,
    /// no project query, no lookup by id. Confirming it is what makes it a
    /// registry entry, and confirming keeps everything the daemon could not
    /// re-derive.
    #[gpui::test]
    async fn test_unconfirmed_rows_are_hidden_until_use() {
        let db = AdeWorkspaceRegistry::open_test_db("test_unconfirmed_rows_are_hidden").await;

        let mut quarantined = workspace("legacy", "project-a");
        quarantined.branch = Some("feature/x".into());
        quarantined.terminal_session_id = Some("ws-1".to_owned());
        db.create_unconfirmed_workspace(quarantined.clone())
            .await
            .unwrap();

        assert!(db.list_workspaces().unwrap().is_empty());
        assert!(
            db.list_workspaces_for_project("project-a".into())
                .unwrap()
                .is_empty()
        );
        assert!(db.get_workspace(quarantined.id.clone()).unwrap().is_none());
        assert_eq!(db.unconfirmed_workspaces().unwrap().len(), 1);

        let used_at = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        db.confirm_workspace(quarantined.id.clone(), used_at)
            .await
            .unwrap();

        let confirmed = db.get_workspace(quarantined.id.clone()).unwrap().unwrap();
        assert_eq!(confirmed.id, quarantined.id);
        assert_eq!(confirmed.branch.as_deref(), Some("feature/x"));
        assert_eq!(confirmed.last_opened_at, used_at);
        assert_eq!(db.list_workspaces().unwrap().len(), 1);
        assert!(db.unconfirmed_workspaces().unwrap().is_empty());
    }
}
