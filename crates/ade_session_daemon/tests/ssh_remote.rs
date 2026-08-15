//! The remote transport end to end, over **real** ssh to `localhost`: start the
//! daemon with `ade-daemon --ensure`, forward its socket with one long-lived
//! `ssh -L`, and speak the whole protocol through that forward.
//!
//! "Remote" is this box — the ssh, the channel multiplexing, the daemon and the
//! forward are all real, only the network is short. That is enough to exercise
//! everything the transport actually depends on.
//!
//! Every test is gated on loopback ssh working, once, and skips otherwise: a
//! machine without a listening sshd must still get a green `cargo test`. This
//! crate can name its own binary through `CARGO_BIN_EXE_ade-daemon`, which is
//! why the daemon-dependent half of the ssh tests lives here rather than beside
//! the rest in `ade_session`.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ade_session::client::Connection;
use ade_session::deploy::HostExec as _;
use ade_session::proto::{Frame, Hello, MAX_GENERATION, SessionId, SessionInfo};
use ade_session::ssh::{EnsureOutcome, HostForward, LocalEndpoint, SshHost, ensure_remote_daemon};
use ade_session_daemon::state::StateStore;
use smol::net::unix::UnixStream;
use tempfile::TempDir;

/// How long any single frame may take to arrive before a test gives up.
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// A session command that stays alive but does nothing.
const IDLE: &str = "sleep 30";

/// `localhost` over ssh, or `None` (having said so) if that is not available
/// here. The probe runs at most once per test binary.
fn loopback() -> Option<SshHost> {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();

    let key = PathBuf::from(std::env::var("HOME").expect("HOME"))
        .join(".ssh")
        .join("id_ed25519_ade_test");
    let host = SshHost::new("localhost").with_extra_args([
        "-i".to_owned(),
        key.display().to_string(),
        "-o".to_owned(),
        "IdentitiesOnly=yes".to_owned(),
        "-o".to_owned(),
        "StrictHostKeyChecking=accept-new".to_owned(),
    ]);
    let available = *AVAILABLE.get_or_init(|| {
        host.run(&["true".to_owned()])
            .is_ok_and(|output| output.success())
    });
    if !available {
        eprintln!("skipping: no loopback ssh");
        return None;
    }
    Some(host)
}

fn daemon_binary() -> String {
    env!("CARGO_BIN_EXE_ade-daemon").to_owned()
}

/// Kills whatever daemon a test caused to be started.
///
/// A daemon started over ssh is in its own session with no parent that outlives
/// it — deliberately unkillable by accident — so the pid file in its state dir
/// is the only handle on it.
struct DaemonGuard {
    state_dir: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(pid) = StateStore::new(&self.state_dir).read_pid() {
            // SAFETY: a plain `kill(2)` on a pid this test caused to exist.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

/// A daemon running "on the host", plus the paths it was told to use.
///
/// The guard is declared **first** so it drops first: the pid file it needs
/// lives inside `dir`, and a `TempDir` that went away before it would take the
/// only handle on the daemon with it.
struct Remote {
    _guard: DaemonGuard,
    dir: TempDir,
    socket: String,
    state: String,
    version_line: String,
}

impl Remote {
    /// `ssh <host> ade-daemon --ensure …` — the one short-lived connection that
    /// precedes the long-lived one.
    fn start(host: &SshHost) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let socket = dir.path().join("daemon.sock").display().to_string();
        let state = dir.path().join("state").display().to_string();
        let guard = DaemonGuard {
            state_dir: PathBuf::from(&state),
        };
        let EnsureOutcome::Listening(version_line) =
            ensure_remote_daemon(host, &daemon_binary(), &socket, &state)
                .expect("ensuring a daemon on the host")
        else {
            panic!("the test's own daemon binary is missing");
        };
        Self {
            _guard: guard,
            dir,
            socket,
            state,
            version_line,
        }
    }

    /// The single ssh connection every channel then rides.
    fn forward(&self, host: &SshHost, name: &str) -> HostForward {
        HostForward::establish(
            host,
            &self.socket,
            LocalEndpoint::Socket(self.dir.path().join(name)),
        )
        .expect("establishing the forward")
    }
}

/// One channel on the forward, handshaken.
async fn connect(forward: &HostForward) -> Connection<UnixStream> {
    let LocalEndpoint::Socket(path) = forward.local() else {
        panic!("these tests forward to a Unix socket");
    };
    let stream = UnixStream::connect(path)
        .await
        .expect("connecting through the forward");
    let mut connection = Connection::new(stream);
    let ack = connection
        .handshake(Hello::current())
        .await
        .expect("handshake through the forward");
    assert_eq!(ack.generation, MAX_GENERATION);
    connection
}

/// `recv()` that fails the test instead of hanging forever. The timeout is a
/// blocking sleep on smol's pool; `smol::Timer` is disallowed by the workspace
/// lints.
async fn recv(connection: &mut Connection<UnixStream>, what: &str) -> Frame {
    let frame = async { Some(connection.recv().await.expect("receiving a frame")) };
    let timeout = async {
        smol::unblock(|| std::thread::sleep(FRAME_TIMEOUT)).await;
        None
    };
    match smol::future::or(frame, timeout).await {
        Some(frame) => frame,
        None => panic!("timed out waiting for {what}"),
    }
}

/// Read frames until `want` accepts one; an attached connection interleaves
/// replies with live output, and no test here cares about that ordering.
async fn reply_until<T>(
    connection: &mut Connection<UnixStream>,
    what: &str,
    want: impl Fn(&Frame) -> Option<T>,
) -> T {
    let deadline = Instant::now() + FRAME_TIMEOUT;
    loop {
        let frame = recv(connection, what).await;
        if let Some(found) = want(&frame) {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
    }
}

async fn create(connection: &mut Connection<UnixStream>, cwd: &Path, command: &str) -> SessionInfo {
    connection
        .send(&Frame::CreateSession {
            workspace_id: "ws-ssh".into(),
            cwd: cwd.display().to_string(),
            command: command.into(),
            env: Vec::new(),
            cols: 80,
            rows: 24,
            agent_kind: "shell".into(),
            instance_label: "forwarded".into(),
            scrollback_bytes: None,
            request_id: Some(7),
        })
        .await
        .expect("sending CreateSession");
    reply_until(connection, "Created", |frame| match frame {
        Frame::Created {
            session,
            request_id,
        } => {
            assert_eq!(*request_id, Some(7));
            Some(session.clone())
        }
        _ => None,
    })
    .await
}

async fn list(connection: &mut Connection<UnixStream>) -> Vec<SessionInfo> {
    connection
        .send(&Frame::ListSessions {
            request_id: Some(11),
        })
        .await
        .expect("sending ListSessions");
    reply_until(connection, "SessionList", |frame| match frame {
        Frame::SessionList {
            sessions,
            request_id,
        } => {
            assert_eq!(*request_id, Some(11));
            Some(sessions.clone())
        }
        _ => None,
    })
    .await
}

async fn kill(connection: &mut Connection<UnixStream>, id: &SessionId) {
    connection
        .send(&Frame::Kill {
            session_id: id.clone(),
            request_id: Some(13),
        })
        .await
        .expect("sending Kill");
    reply_until(connection, "Removed", |frame| match frame {
        Frame::Removed { session_id } if session_id == id => Some(()),
        _ => None,
    })
    .await;
}

/// Attach and keep reading until `needle` has been seen, starting from the
/// replay. Whether it arrives in the replay or in live output is a race the
/// test has no business caring about; that it arrives through the ssh channel
/// is the point.
async fn attach_until(
    connection: &mut Connection<UnixStream>,
    id: &SessionId,
    needle: &[u8],
) -> Vec<u8> {
    connection
        .send(&Frame::Attach {
            session_id: id.clone(),
            request_id: Some(21),
        })
        .await
        .expect("sending Attach");
    let mut seen = reply_until(connection, "Replay", |frame| match frame {
        Frame::Replay {
            session_id, bytes, ..
        } if session_id == id => Some(bytes.clone()),
        _ => None,
    })
    .await;
    while !contains(&seen, needle) {
        let bytes = reply_until(connection, "Output", |frame| match frame {
            Frame::Output { session_id, bytes } if session_id == id => Some(bytes.clone()),
            _ => None,
        })
        .await;
        seen.extend_from_slice(&bytes);
    }
    seen
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn has(sessions: &[SessionInfo], id: &SessionId) -> bool {
    sessions.iter().any(|session| &session.id == id)
}

/// 1. Arguments survive the trip through ssh and a remote login shell exactly
///    as written — spaces, quotes and all.
#[test]
fn run_echoes_argv_over_ssh_with_spaces_and_quotes_intact() {
    let Some(host) = loopback() else { return };

    let output = host
        .run(&[
            "printf".to_owned(),
            "[%s]".to_owned(),
            "a b".to_owned(),
            "it's".to_owned(),
            "$HOME".to_owned(),
        ])
        .expect("running printf over ssh");

    assert!(output.success(), "{}", output.stderr);
    assert_eq!(output.stdout, "[a b][it's][$HOME]");
}

/// 2. Upload streams bytes over the same channel and lands them executable.
#[test]
fn upload_writes_an_executable_on_the_host() {
    let Some(host) = loopback() else { return };
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("bin").join("ade-daemon");
    let bytes = b"#!/bin/sh\nprintf 'ade_session_daemon 9.9.9\\n'\n";

    host.upload(bytes, &path.display().to_string())
        .expect("uploading over ssh");

    assert_eq!(fs::read(&path).expect("reading it back"), bytes);
    assert_eq!(
        fs::metadata(&path).expect("stat").permissions().mode() & 0o777,
        0o755
    );
}

/// 3. `--ensure` is start-if-absent: the first call brings a daemon up, the
///    second finds that same one, and both answer with its version.
#[test]
fn ensure_starts_a_daemon_and_a_second_ensure_finds_it() {
    let Some(host) = loopback() else { return };
    let remote = Remote::start(&host);
    let pid = StateStore::new(&remote.state)
        .read_pid()
        .expect("the started daemon recorded its pid");

    let again = ensure_remote_daemon(&host, &daemon_binary(), &remote.socket, &remote.state)
        .expect("a second --ensure");

    assert!(
        remote.version_line.starts_with("ade-daemon "),
        "got {:?}",
        remote.version_line
    );
    assert_eq!(again, EnsureOutcome::Listening(remote.version_line.clone()));
    assert_eq!(
        StateStore::new(&remote.state).read_pid(),
        Some(pid),
        "no second daemon was started"
    );
}

/// 4. The whole protocol over one forwarded socket: handshake, create, list and
///    an attach replay, every byte of it through an ssh channel.
#[test]
fn the_forward_carries_the_whole_protocol() {
    let Some(host) = loopback() else { return };
    let remote = Remote::start(&host);
    let mut forward = remote.forward(&host, "local.sock");

    smol::block_on(async {
        let mut connection = connect(&forward).await;
        let session = create(
            &mut connection,
            remote.dir.path(),
            "sh -c 'printf FORWARDED; sleep 30'",
        )
        .await;
        assert!(has(&list(&mut connection).await, &session.id));

        let output = attach_until(&mut connection, &session.id, b"FORWARDED").await;
        assert!(contains(&output, b"FORWARDED"));

        // A second connection is a second channel on the same ssh process.
        let mut other = connect(&forward).await;
        assert!(has(&list(&mut other).await, &session.id));

        kill(&mut other, &session.id).await;
    });

    assert!(forward.is_alive(), "one ssh process served all of that");
}

/// 5. Dropping the forward is a detach: the ssh connection and every channel on
///    it go away, and the daemon keeps its sessions for the next one.
#[test]
fn the_daemon_outlives_the_forward_that_created_its_session() {
    let Some(host) = loopback() else { return };
    let remote = Remote::start(&host);
    let pid = StateStore::new(&remote.state).read_pid();

    let session = {
        let forward = remote.forward(&host, "first.sock");
        smol::block_on(async {
            let mut connection = connect(&forward).await;
            create(&mut connection, remote.dir.path(), IDLE).await
        })
        // `forward` drops here: ssh is killed, every channel with it.
    };

    let forward = remote.forward(&host, "second.sock");
    smol::block_on(async {
        let mut connection = connect(&forward).await;
        assert_eq!(
            StateStore::new(&remote.state).read_pid(),
            pid,
            "the same daemon is still serving"
        );
        assert!(
            has(&list(&mut connection).await, &session.id),
            "the session outlived the forward it was created through"
        );
        kill(&mut connection, &session.id).await;
    });
}
