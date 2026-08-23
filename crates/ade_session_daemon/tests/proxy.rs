//! Integration tests for `--stdio-proxy`, driving the **real** built binary as
//! a child process and speaking the protocol over its stdio — the same shape
//! `ssh <host> ade-daemon --stdio-proxy` has, minus the ssh.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ade_session::proto::{Frame, Hello, MAX_GENERATION, SessionId, SessionInfo};
use ade_session::transport::ChildConnection;
use ade_session_daemon::state::StateStore;
use ade_session_daemon::{Server, ServerConfig};
use tempfile::TempDir;

/// How long any single frame may take to arrive before a test gives up.
const FRAME_TIMEOUT: Duration = Duration::from_secs(45);

/// A session command that stays alive but does nothing.
const IDLE: &str = "sleep 30";

/// Spawn the real binary in proxy mode, pointed at `socket`.
fn proxy(socket: &Path, state_dir: &Path) -> ChildConnection {
    let argv = vec![
        env!("CARGO_BIN_EXE_ade-daemon").to_owned(),
        "--stdio-proxy".to_owned(),
        "--socket".to_owned(),
        socket.display().to_string(),
        "--state-dir".to_owned(),
        state_dir.display().to_string(),
    ];
    ChildConnection::spawn(&argv).expect("spawning the proxy")
}

/// A proxy that has completed the handshake, i.e. one whose bytes really did
/// reach a daemon and come back.
async fn connected(socket: &Path, state_dir: &Path) -> ChildConnection {
    let mut connection = proxy(socket, state_dir);
    let ack = connection
        .handshake(Hello::current())
        .await
        .expect("handshake through the proxy");
    assert_eq!(ack.generation, MAX_GENERATION);
    connection
}

/// `recv()` that fails the test instead of hanging forever.
///
/// The timeout is a blocking sleep on smol's blocking pool rather than a
/// `smol::Timer`, which the workspace lints disallow.
async fn recv(connection: &mut ChildConnection, what: &str) -> Frame {
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

/// Read frames until `want` accepts one, ignoring everything else.
///
/// Ignoring is the point: once a connection is attached, its replies arrive
/// interleaved with live `Output`, and no test here cares about the ordering
/// between the two.
async fn reply_until<T>(
    connection: &mut ChildConnection,
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

/// A workspace to put sessions in: the daemon holds no record for an id it was
/// never asked to create, and refuses a session naming one.
async fn workspace(connection: &mut ChildConnection, root: &Path) -> String {
    connection
        .send(&Frame::CreateWorkspace {
            root: root.display().to_string(),
            name: Some("proxied".into()),
            project_id: None,
            project_identity: None,
            request_id: Some(6),
            env: Vec::new(),
            cols: None,
            rows: None,
        })
        .await
        .expect("sending CreateWorkspace");
    reply_until(connection, "Workspace", |frame| match frame {
        Frame::Workspace {
            workspace,
            request_id: Some(6),
            ..
        } => Some(workspace.id.clone()),
        _ => None,
    })
    .await
}

async fn create(connection: &mut ChildConnection, cwd: &Path, command: &str) -> SessionInfo {
    let workspace_id = workspace(connection, cwd).await;
    connection
        .send(&Frame::CreateSession {
            workspace_id,
            cwd: cwd.display().to_string(),
            project_id: None,
            project_identity: None,
            command: command.into(),
            env: Vec::new(),
            cols: 80,
            rows: 24,
            agent_kind: "shell".into(),
            instance_label: "proxied".into(),
            scrollback_bytes: None,
            request_id: Some(7),
        })
        .await
        .expect("sending CreateSession");
    reply_until(connection, "Created", |frame| match frame {
        Frame::Created {
            session,
            request_id,
            ..
        } => {
            assert_eq!(*request_id, Some(7));
            Some(session.clone())
        }
        _ => None,
    })
    .await
}

async fn list(connection: &mut ChildConnection) -> Vec<SessionInfo> {
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

async fn kill(connection: &mut ChildConnection, id: &SessionId) {
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
/// test has no business caring about; that it arrives is the point.
async fn attach_until(connection: &mut ChildConnection, id: &SessionId, needle: &[u8]) -> Vec<u8> {
    connection
        .send(&Frame::Attach {
            session_id: id.clone(),
            view_id: None,
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

/// Kills whatever daemon a test caused to be started.
///
/// A proxy-started daemon is deliberately unkillable by accident — its own
/// session, no parent that outlives it — so a test that starts one has to go
/// and find it. The pid file the daemon writes into its state dir is how.
struct DaemonGuard {
    state_dir: PathBuf,
}

impl DaemonGuard {
    fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
        }
    }

    fn pid(&self) -> Option<u32> {
        StateStore::new(&self.state_dir).read_pid()
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid() {
            // SAFETY: a plain `kill(2)` on a pid this test caused to exist.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

/// Everything a proxy test needs: a temp dir and the two paths inside it.
struct Paths {
    dir: TempDir,
    socket: PathBuf,
    state: PathBuf,
}

fn paths() -> Paths {
    let dir = TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    let state = dir.path().join("state");
    Paths { dir, socket, state }
}

/// The whole protocol survives a round trip through the proxy's stdio: nothing
/// in it parses a frame, so nothing in it can reorder or corrupt one.
#[test]
fn the_proxy_carries_the_protocol_to_a_running_daemon() {
    let paths = paths();
    let server = Server::spawn(ServerConfig::new(&paths.socket, &paths.state))
        .expect("spawning the daemon in-process");
    smol::block_on(async {
        let mut connection = connected(server.socket_path(), &paths.state).await;

        let session = create(
            &mut connection,
            paths.dir.path(),
            "sh -c 'printf PROXIED; sleep 30'",
        )
        .await;
        assert!(has(&list(&mut connection).await, &session.id));

        let output = attach_until(&mut connection, &session.id, b"PROXIED").await;
        assert!(contains(&output, b"PROXIED"));

        kill(&mut connection, &session.id).await;
        let status = connection.shutdown().await.expect("proxy exit status");
        assert!(status.success(), "the proxy exits 0, got {status:?}");
    });
}

/// Start-if-absent, and the single-daemon-per-host rule that follows from it:
/// the second proxy must find the first one's daemon, not start its own.
#[test]
fn a_proxy_starts_a_daemon_and_a_second_proxy_reuses_it() {
    let paths = paths();
    let guard = DaemonGuard::new(&paths.state);
    assert!(!paths.socket.exists(), "nothing is listening yet");

    smol::block_on(async {
        let mut first = connected(&paths.socket, &paths.state).await;
        let session = create(&mut first, paths.dir.path(), IDLE).await;
        let pid = guard.pid().expect("the started daemon recorded its pid");

        let mut second = connected(&paths.socket, &paths.state).await;
        assert!(
            has(&list(&mut second).await, &session.id),
            "the second proxy reached the same daemon"
        );
        assert_eq!(guard.pid(), Some(pid), "no second daemon was started");

        kill(&mut second, &session.id).await;
        first.shutdown().await.expect("first proxy exit status");
        second.shutdown().await.expect("second proxy exit status");
    });
}

/// The proxy dying is a detach and nothing more — which is the entire point of
/// putting the daemon behind one.
#[test]
fn closing_the_proxy_leaves_the_daemon_and_its_sessions_running() {
    let paths = paths();
    let guard = DaemonGuard::new(&paths.state);

    smol::block_on(async {
        let mut connection = connected(&paths.socket, &paths.state).await;
        let session = create(&mut connection, paths.dir.path(), IDLE).await;
        let pid = guard.pid().expect("the started daemon recorded its pid");

        // Closing stdin is what ssh does when its channel goes away.
        let status = connection.shutdown().await.expect("proxy exit status");
        assert!(status.success(), "the proxy exits 0 on stdin EOF");

        let mut reconnected = connected(&paths.socket, &paths.state).await;
        assert_eq!(guard.pid(), Some(pid), "the same daemon is still serving");
        assert!(
            has(&list(&mut reconnected).await, &session.id),
            "the session outlived the proxy that created it"
        );

        kill(&mut reconnected, &session.id).await;
        reconnected.shutdown().await.expect("proxy exit status");
    });
}
