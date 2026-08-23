//! Session *metadata* and workspace persistence.
//!
//! A PTY cannot outlive the daemon and cannot be resurrected, so the session
//! half of this file is not a restore point — it exists so that a daemon which
//! restarts can say "these sessions existed and are gone" instead of silently
//! forgetting them. Only the fields needed to name a dead session are stored;
//! no scrollback, no process state.
//!
//! **Workspaces are different: they really are restored.** A workspace's name,
//! project root, `project_scope_rev`, layout and `layout_rev` all survive a
//! restart, because none of them is tied to a live process — only the terminal
//! tabs inside the layout are, and those are pruned on load (see
//! [`SessionTable::load`](crate::sessions::SessionTable::load)).
//!
//! Writes are atomic *and* durable (temp file, fsync, rename, fsync the
//! directory) so a crash mid-write leaves either the old file or the new one,
//! never a truncated one — see [`write_atomic`] for why the rename alone is
//! not enough.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ade_session::proto::{SessionId, SessionInfo, WorkspaceInfo};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// Schema version of `sessions.json`. Bump only on a breaking change; new
/// fields should be `Option` / `#[serde(default)]` instead.
pub const STATE_VERSION: u32 = 1;

/// Name of the state file inside the state dir.
pub const STATE_FILE: &str = "sessions.json";

/// Name of the pid file inside the state dir.
///
/// Written so that a daemon started headlessly — by [`proxy`](crate::proxy)'s
/// start-if-absent, with no terminal and no parent that outlives it — can
/// still be found. It is **not** a liveness check: the socket is, and a stale
/// pid file is the normal outcome of a crash or of a daemon that lost the
/// already-running race. Nothing in the daemon ever reads it.
pub const PID_FILE: &str = "daemon.pid";

/// Name of the file holding this daemon's [instance id](StateStore::instance_id).
pub const INSTANCE_FILE: &str = "instance.id";

/// One session as recorded on disk. Deliberately a subset of
/// [`SessionInfo`]: status is not persisted, because after a restart the only
/// honest status is "gone".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedSession {
    pub id: SessionId,
    /// Absent in files written before workspaces existed; such a session is
    /// wrapped in a workspace of its own on load.
    #[serde(default)]
    pub workspace_id: String,
    pub agent_kind: String,
    pub instance_label: String,
    pub cwd: String,
    /// Unix seconds.
    pub created_at: u64,
}

impl PersistedSession {
    pub fn from_info(info: &SessionInfo) -> Self {
        Self {
            id: info.id.clone(),
            workspace_id: info.workspace_id.clone(),
            agent_kind: info.agent_kind.clone(),
            instance_label: info.instance_label.clone(),
            cwd: info.cwd.clone(),
            created_at: info.created_at,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct StateFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    sessions: Vec<PersistedSession>,
    #[serde(default)]
    workspaces: Vec<WorkspaceInfo>,
}

/// Everything a previous daemon left behind.
#[derive(Clone, Debug)]
pub struct PersistedState {
    pub sessions: Vec<PersistedSession>,
    pub workspaces: Vec<WorkspaceInfo>,
    /// Whether these rows are the whole truth about what the previous daemon
    /// left: a file that parsed, or no file at all on a genuine first run.
    /// `false` means the file exists and could not be read or understood, so
    /// the empty lists above are this build's ignorance and not the host's
    /// state. **Nothing may be destroyed on the strength of a `false` here** —
    /// an unmatched terminal is only an orphan if the ledger that would have
    /// named it was readable.
    pub authoritative: bool,
}

impl Default for PersistedState {
    /// The first-run state: nothing recorded, and that is a fact.
    fn default() -> Self {
        Self {
            sessions: Vec::new(),
            workspaces: Vec::new(),
            authoritative: true,
        }
    }
}

/// Reads and atomically rewrites `<dir>/sessions.json`.
#[derive(Debug)]
pub struct StateStore {
    dir: PathBuf,
    /// Set by [`StateStore::load`] when the file on disk is from a *newer*
    /// schema than this build understands. See [`StateStore::is_degraded`].
    degraded: AtomicBool,
    /// Set by [`StateStore::load`] when a file that exists could not be read or
    /// understood — the [`PersistedState::authoritative`] `false` case. Kept
    /// apart from `degraded` because the two are refused for different reasons
    /// and each says so once, in its own words; the wire sees only their union
    /// ([`StateStore::read_only`]).
    unreadable: AtomicBool,
}

impl StateStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            degraded: AtomicBool::new(false),
            unreadable: AtomicBool::new(false),
        }
    }

    /// True once a newer-schema `sessions.json` has been seen: sessions still
    /// run, but nothing is written back to the ledger.
    ///
    /// A newer file is a file a newer daemon owns. Rewriting it at our schema
    /// would silently drop every field we do not know about — the newer
    /// daemon's workspaces, its layouts — so the only safe move is to read
    /// what we can and never write. Exposed so the server can report the mode;
    /// the wire protocol does not carry it yet.
    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Relaxed)
    }

    /// True while this store refuses to write, for either reason: the file
    /// belongs to a newer schema, or it exists and could not be read.
    ///
    /// The second is the one a rewrite would *destroy*: the rows a corrupt or
    /// unreadable file still holds are the only record of what runs on this
    /// host, and saving over them turns a transient read failure into permanent
    /// loss. Both are one condition to everyone outside — the guard on
    /// [`Self::save`] and the `persisted` flag every ack carries (§8.5).
    pub fn read_only(&self) -> bool {
        self.is_degraded() || self.unreadable.load(Ordering::Relaxed)
    }

    /// Enter degraded mode, logging only on the transition so that the
    /// refusal is stated once rather than on every save.
    fn degrade(&self, path: &Path, version: u32) {
        if !self.degraded.swap(true, Ordering::Relaxed) {
            log::warn!(
                "{} is schema version {version}, newer than this daemon's {STATE_VERSION}: \
                 running with persistence disabled, the file will not be rewritten",
                path.display()
            );
        }
    }

    /// The other half of [`Self::read_only`], stated once like [`Self::degrade`]
    /// and for its own reason: the rows an unreadable file still holds may name
    /// terminals that are running.
    fn mark_unreadable(&self, path: &Path) {
        if !self.unreadable.swap(true, Ordering::Relaxed) {
            log::warn!(
                "not writing to {}: overwriting a ledger this daemon could not read would \
                 destroy whatever it still records",
                path.display()
            );
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(STATE_FILE)
    }

    pub fn pid_path(&self) -> PathBuf {
        self.dir.join(PID_FILE)
    }

    /// Record this process's pid — see [`PID_FILE`].
    pub fn write_pid(&self) -> Result<()> {
        create_private_dir(&self.dir)?;
        let path = self.pid_path();
        fs::write(&path, format!("{}\n", std::process::id()))
            .with_context(|| format!("writing {}", path.display()))
    }

    /// The pid recorded by whichever daemon last bound here, if any. A pid
    /// found here may well be dead; callers must treat it as a hint.
    pub fn read_pid(&self) -> Option<u32> {
        fs::read_to_string(self.pid_path())
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    pub fn remove_pid(&self) {
        let _ = fs::remove_file(self.pid_path());
    }

    pub fn instance_path(&self) -> PathBuf {
        self.dir.join(INSTANCE_FILE)
    }

    /// **Which daemon this is**, across restarts: a uuid read from
    /// `<dir>/instance.id`, minted and written on first use.
    ///
    /// A file of its own, not a field of the ledger, for two reasons that are
    /// one: the ledger is refused to a daemon whose schema is too old
    /// ([`Self::read_only`]) and that daemon still has to say who it is, and a
    /// client's identity for a host must not move when the ledger is replaced.
    ///
    /// A directory it cannot write leaves the daemon with a fresh id per
    /// start — the client then sees a new daemon after every restart, which is
    /// no worse than the host-spelling identity it falls back to without the
    /// field at all.
    pub fn instance_id(&self) -> String {
        let path = self.instance_path();
        if let Ok(recorded) = fs::read_to_string(&path) {
            let recorded = recorded.trim();
            if !recorded.is_empty() {
                return recorded.to_owned();
            }
        }
        let minted = uuid::Uuid::new_v4().to_string();
        if let Err(err) = create_private_dir(&self.dir).and_then(|()| {
            fs::write(&path, format!("{minted}\n"))
                .with_context(|| format!("writing {}", path.display()))
        }) {
            log::warn!(
                "could not record this daemon's instance id, so clients will see a different \
                 daemon after every restart: {err:#}"
            );
        }
        minted
    }

    /// Sessions and workspaces recorded by a previous daemon. A missing file is
    /// not an error — it is the normal first-run case. A *corrupt* file is
    /// logged and treated as empty, because refusing to start would be worse
    /// than losing a list of already-dead sessions — but the store then refuses
    /// every write too ([`StateStore::read_only`]), so "treated as empty" never
    /// becomes "made empty".
    ///
    /// A file from a **newer** schema is the one case that is neither: it is
    /// read as far as it parses, and the store then refuses every write for the
    /// rest of the process (see [`StateStore::is_degraded`]). "Treat as empty"
    /// would be the one outcome worse than not starting — the next save would
    /// overwrite a newer daemon's ledger with our own idea of it.
    ///
    /// Both "treat as empty" outcomes carry
    /// [`PersistedState::authoritative`] `false` — the distinction a caller
    /// that would *destroy* what the ledger does not name has to make.
    pub fn load(&self) -> PersistedState {
        let path = self.path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return PersistedState::default();
            }
            Err(err) => {
                log::warn!("could not read {}: {err}", path.display());
                self.mark_unreadable(&path);
                return PersistedState {
                    authoritative: false,
                    ..PersistedState::default()
                };
            }
        };
        match serde_json::from_slice::<StateFile>(&bytes) {
            Ok(state) => {
                if state.version > STATE_VERSION {
                    self.degrade(&path, state.version);
                }
                PersistedState {
                    sessions: state.sessions,
                    workspaces: state.workspaces,
                    authoritative: true,
                }
            }
            Err(err) => {
                // A future file may well be unparseable *because* it is from
                // the future, so the version is dug out of the bytes before
                // the file is written off as corrupt.
                match sniff_version(&bytes) {
                    Some(version) if version > STATE_VERSION => self.degrade(&path, version),
                    _ => {
                        log::warn!("ignoring malformed {}: {err}", path.display());
                        self.mark_unreadable(&path);
                    }
                }
                PersistedState {
                    authoritative: false,
                    ..PersistedState::default()
                }
            }
        }
    }

    /// Write the whole state atomically and durably — see [`write_atomic`].
    ///
    /// A no-op while the store is [`StateStore::read_only`]: the guard is here,
    /// on the store's only write path, so neither a newer-schema file nor one
    /// this daemon could not read can be overwritten by any caller. It reports
    /// success because the refusal is a standing condition already logged once
    /// by [`StateStore::load`], not a per-save failure for callers to retry or
    /// log again.
    pub fn save(&self, sessions: &[PersistedSession], workspaces: &[WorkspaceInfo]) -> Result<()> {
        if self.read_only() {
            return Ok(());
        }
        create_private_dir(&self.dir)?;
        let state = StateFile {
            version: STATE_VERSION,
            sessions: sessions.to_vec(),
            workspaces: workspaces.to_vec(),
        };
        let json = serde_json::to_vec_pretty(&state).context("serializing session state")?;
        write_atomic(&self.path(), &json)
    }
}

/// Dig the `version` field out of a file serde could not deserialize.
///
/// Only ever used to decide whether an unparseable file is from the future and
/// must therefore be left alone; a `None` here means "no readable version",
/// which is the existing corrupt-file case.
fn sniff_version(bytes: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(bytes).ok()?;
    let after_key = text.split_once("\"version\"")?.1;
    let digits: String = after_key
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Replace `path`'s contents with `bytes` so that a crash at any point leaves
/// either the whole old file or the whole new one.
///
/// Synchronous on purpose: this is the primitive, and its callers decide
/// whether they can afford to block.
///
/// `fs::write` + `fs::rename` is *atomic* but not *durable*. The rename is a
/// metadata operation and can reach the disk before the temp file's data does,
/// so a crash in that window leaves the destination name pointing at a file
/// with no contents — the ledger is lost exactly when it is needed. The order
/// below closes it:
///
/// 1. write the temp file, then `sync_all` — the data is on disk before any
///    name points at it;
/// 2. rename over the destination — atomic on both platforms (`rename(2)`;
///    `MOVEFILE_REPLACE_EXISTING` on Windows, which is why no explicit
///    `ReplaceFile` call is needed);
/// 3. fsync the parent directory, so the rename itself is durable.
///
/// Step 3 is a Unix-only concept. Windows has no directory handle to flush;
/// NTFS journals the rename's metadata, so the ordering guarantee of steps 1–2
/// is what carries the durability there.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);

    let write = |temp: &Path| -> Result<()> {
        let mut file =
            File::create(temp).with_context(|| format!("creating {}", temp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing {}", temp.display()))?;
        file.sync_all()
            .with_context(|| format!("flushing {}", temp.display()))
    };
    if let Err(err) = write(&temp) {
        let _ = fs::remove_file(&temp);
        return Err(err);
    }

    if let Err(err) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(anyhow::Error::new(err).context(format!(
            "renaming {} to {}",
            temp.display(),
            path.display()
        )));
    }

    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        // Best effort: a directory fsync is rejected outright by some
        // filesystems, and the new contents are already on disk either way —
        // only the durability of the rename is at stake, so a failure here is
        // not worth failing a save that otherwise succeeded.
        match File::open(dir).and_then(|dir| dir.sync_all()) {
            Ok(()) => {}
            Err(err) => log::debug!("could not fsync {}: {err}", dir.display()),
        }
    }
    Ok(())
}

/// `mkdir -p` with mode 0700 — the socket and the session list are per-user
/// state and have no business being world-readable.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting mode 0700 on {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> PersistedSession {
        PersistedSession {
            id: SessionId::from(id.to_string()),
            workspace_id: "ws".into(),
            agent_kind: "claude".into(),
            instance_label: id.into(),
            cwd: "/tmp".into(),
            created_at: 1,
        }
    }

    /// **Which daemon this is** outlives the process, and does not depend on
    /// the ledger: a daemon that refuses to write `sessions.json` still has to
    /// tell a client which host it is, or two spellings of that host go back to
    /// being two.
    #[test]
    fn the_instance_id_survives_a_restart_and_a_refused_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let minted = StateStore::new(dir.path()).instance_id();
        assert!(!minted.is_empty());
        assert_eq!(
            StateStore::new(dir.path()).instance_id(),
            minted,
            "the next daemon on this state dir is the same daemon"
        );

        let refusing = StateStore::new(dir.path());
        std::fs::write(refusing.path(), FUTURE).unwrap();
        refusing.load();
        assert!(refusing.read_only(), "the ledger is a newer schema");
        assert_eq!(refusing.instance_id(), minted);
    }

    #[test]
    fn write_atomic_creates_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        write_atomic(&path, b"first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");
    }

    #[test]
    fn write_atomic_overwrites_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second, and longer").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second, and longer");
        assert!(!dir.path().join("ledger.json.tmp").exists());
    }

    #[test]
    fn save_roundtrips_through_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path());
        store.save(&[session("a"), session("b")], &[]).unwrap();
        let loaded = StateStore::new(dir.path()).load();
        assert_eq!(loaded.sessions, vec![session("a"), session("b")]);
        assert!(!store.is_degraded());
    }

    /// A well-formed ledger from a schema this build does not know, carrying a
    /// field it has never heard of.
    const FUTURE: &str = r#"{
  "version": 99,
  "sessions": [
    {
      "id": "from-the-future",
      "workspace_id": "ws",
      "agent_kind": "claude",
      "instance_label": "one",
      "cwd": "/tmp",
      "created_at": 1,
      "unknown_future_field": {"keep": "me"}
    }
  ],
  "workspaces": [],
  "unknown_future_table": []
}"#;

    fn seeded(contents: &str) -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(STATE_FILE), contents).unwrap();
        let store = StateStore::new(dir.path());
        (dir, store)
    }

    #[test]
    fn a_newer_schema_survives_a_save_byte_for_byte() {
        let (dir, store) = seeded(FUTURE);
        store.load();
        assert!(store.is_degraded());
        store.save(&[session("a")], &[]).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(STATE_FILE)).unwrap(),
            FUTURE
        );
    }

    #[test]
    fn a_degraded_store_still_loads_its_sessions() {
        let (_dir, store) = seeded(FUTURE);
        let loaded = store.load();
        assert!(store.is_degraded());
        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.sessions[0].id.0, "from-the-future");
    }

    #[test]
    fn a_malformed_file_with_a_readable_future_version_is_left_alone() {
        let truncated = "{\n  \"version\": 99,\n  \"sessions\": [{\"id\": \"a\"";
        let (dir, store) = seeded(truncated);
        assert!(store.load().sessions.is_empty());
        assert!(store.is_degraded());
        store.save(&[session("a")], &[]).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(STATE_FILE)).unwrap(),
            truncated
        );
    }

    /// A file that exists and did not parse may still be the only record of
    /// what runs on this host — a transient read failure, a half-written file —
    /// so it survives a save byte for byte, the way a newer schema does. The
    /// two reasons stay apart internally and are one condition on the wire.
    #[test]
    fn an_unreadable_file_defaults_and_is_left_alone() {
        let (dir, store) = seeded("this was never json");
        assert!(store.load().sessions.is_empty());
        assert!(!store.is_degraded(), "malformed is not newer-schema");
        assert!(store.read_only());
        store.save(&[session("a")], &[]).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(STATE_FILE)).unwrap(),
            "this was never json"
        );
    }

    /// The read-only union both `save` and the `persisted` flag are taken from.
    #[test]
    fn read_only_covers_both_refusals_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path());
        assert!(!store.read_only(), "a first run writes normally");
        store.load();
        assert!(!store.read_only());

        let (_dir, future) = seeded(FUTURE);
        future.load();
        assert!(future.read_only());

        // Unreadable for a reason that is not a parse failure: the name is a
        // directory, so there are no bytes at all.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(STATE_FILE)).unwrap();
        let store = StateStore::new(dir.path());
        store.load();
        assert!(store.read_only());
    }

    #[test]
    fn our_own_version_is_not_from_the_future() {
        let (_dir, store) = seeded(&format!(
            r#"{{"version": {STATE_VERSION}, "sessions": []}}"#
        ));
        store.load();
        assert!(!store.is_degraded());
    }

    /// The flag a later increment gates killing unmatched terminals on. Only
    /// the two outcomes that really are the whole truth may claim to be it: a
    /// file that parsed, and no file at all.
    #[test]
    fn only_a_readable_ledger_is_authoritative() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            StateStore::new(dir.path()).load().authoritative,
            "a first run knows there is nothing, which is a fact and not a gap"
        );

        StateStore::new(dir.path())
            .save(&[session("a")], &[])
            .unwrap();
        let loaded = StateStore::new(dir.path()).load();
        assert!(loaded.authoritative);
        assert_eq!(loaded.sessions.len(), 1);

        let (_dir, store) = seeded("this was never json");
        assert!(
            !store.load().authoritative,
            "an unreadable ledger is ignorance, not an empty host"
        );

        // Unreadable for a different reason: the name is a directory, so there
        // are no bytes to parse and no first run either.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(STATE_FILE)).unwrap();
        assert!(!StateStore::new(dir.path()).load().authoritative);

        // A newer schema is readable *and* authoritative: those rows are real,
        // it is only writing them back that is refused.
        let (_dir, store) = seeded(FUTURE);
        assert!(store.load().authoritative);
        assert!(store.is_degraded());
    }

    #[test]
    fn sniff_version_reads_only_a_readable_version() {
        assert_eq!(sniff_version(br#"{"version" : 42, "#), Some(42));
        assert_eq!(sniff_version(br#"{"sessions": []}"#), None);
        assert_eq!(sniff_version(br#"{"version": "two"}"#), None);
    }
}
