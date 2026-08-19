//! Integration tests driving a real server in-process over a real Unix socket
//! in a temp dir. The client is `ade_session::Connection`, i.e. exactly what
//! the app will use.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use ade_session::client::Connection;
use ade_session::framing::ReadFrameError;
use ade_session::proto::{
    Frame, Hello, LayoutDoc, LayoutNode, MAX_GENERATION, MIN_GENERATION, SessionId, SessionInfo,
    SessionStatus, SplitDir, Tab, WorkspaceInfo, error_code,
};
use ade_session_daemon::{RunningServer, Server, ServerConfig, StatusConfig};
use smol::io::AsyncWriteExt as _;
use smol::net::unix::UnixStream;
use tempfile::TempDir;

/// A server on a socket inside a fresh temp dir, with its state dir alongside.
fn server() -> (TempDir, RunningServer) {
    let dir = TempDir::new().expect("temp dir");
    let running = server_in(&dir);
    (dir, running)
}

fn server_in(dir: &TempDir) -> RunningServer {
    server_named(dir, "daemon.sock")
}

fn server_named(dir: &TempDir, socket: &str) -> RunningServer {
    let config = ServerConfig::new(dir.path().join(socket), dir.path().join("state"));
    Server::spawn(config).expect("spawning server")
}

/// Status derivation tuned down so that a five-second rule is observable in a
/// fraction of a second. The ratio to the sweep interval is what keeps these
/// tests honest rather than fast: 300ms of silence is still six sweeps.
const FAST: StatusConfig = StatusConfig {
    needs_input_after: Duration::from_millis(300),
    sweep_interval: Duration::from_millis(50),
};

/// Silence long enough that nothing in a test can reach it, so that a
/// `NeedsInput` can only have come from a bell.
const BELL_ONLY: StatusConfig = StatusConfig {
    needs_input_after: Duration::from_secs(600),
    sweep_interval: Duration::from_millis(50),
};

/// A server whose status derivation runs on `status`.
fn tuned_server(status: StatusConfig) -> (TempDir, RunningServer) {
    let dir = TempDir::new().expect("temp dir");
    let config = ServerConfig::new(dir.path().join("daemon.sock"), dir.path().join("state"))
        .with_status(status);
    let running = Server::spawn(config).expect("spawning server");
    (dir, running)
}

fn fast_server() -> (TempDir, RunningServer) {
    tuned_server(FAST)
}

async fn client(socket: &Path) -> Connection<UnixStream> {
    let (connection, _raw) = raw_client(socket).await;
    connection
}

/// A handshaken connection, plus a second handle on the same socket.
///
/// The raw handle is how the tests below put an envelope on the wire that the
/// codec would never emit — an op this build has never heard of, a body of the
/// wrong shape. [`Connection`] can only send frames that already exist as
/// [`Frame`] variants, which is exactly the set that cannot exercise §2.
async fn raw_client(socket: &Path) -> (Connection<UnixStream>, UnixStream) {
    let stream = UnixStream::connect(socket).await.expect("connecting");
    let raw = stream.clone();
    let mut connection = Connection::new(stream);
    let ack = connection
        .handshake(Hello::current())
        .await
        .expect("handshake");
    assert_eq!(
        ack.generation, MAX_GENERATION,
        "the daemon selects the generation and this build has exactly one"
    );
    assert_eq!(
        ack.protocol_version, ack.generation,
        "the legacy field is the selected generation and nothing else"
    );
    (connection, raw)
}

/// Put a hand-built payload on the wire behind its 4-byte length prefix.
async fn send_raw(stream: &mut UnixStream, payload: &[u8]) {
    let mut framed = Vec::with_capacity(4 + payload.len());
    framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    framed.extend_from_slice(payload);
    stream
        .write_all(&framed)
        .await
        .expect("writing a raw frame");
    stream.flush().await.expect("flushing a raw frame");
}

fn create_frame(cwd: &Path, command: &str, request_id: u64, scrollback: Option<u64>) -> Frame {
    Frame::CreateSession {
        workspace_id: "ws-1".into(),
        cwd: cwd.display().to_string(),
        command: command.into(),
        env: vec![("ADE_TEST".into(), "1".into())],
        cols: 80,
        rows: 24,
        agent_kind: "shell".into(),
        instance_label: "test".into(),
        scrollback_bytes: scrollback,
        request_id: Some(request_id),
    }
}

async fn create(connection: &mut Connection<UnixStream>, cwd: &Path, command: &str) -> SessionInfo {
    create_with_scrollback(connection, cwd, command, None).await
}

async fn create_with_scrollback(
    connection: &mut Connection<UnixStream>,
    cwd: &Path,
    command: &str,
    scrollback: Option<u64>,
) -> SessionInfo {
    connection
        .send(&create_frame(cwd, command, 7, scrollback))
        .await
        .expect("sending CreateSession");
    match connection.recv().await.expect("reply") {
        Frame::Created {
            session,
            request_id,
        } => {
            assert_eq!(request_id, Some(7));
            session
        }
        other => panic!("expected Created, got {other:?}"),
    }
}

async fn list(connection: &mut Connection<UnixStream>) -> Vec<SessionInfo> {
    connection
        .send(&Frame::ListSessions {
            request_id: Some(11),
        })
        .await
        .expect("sending ListSessions");
    match connection.recv().await.expect("reply") {
        Frame::SessionList {
            sessions,
            request_id,
        } => {
            assert_eq!(request_id, Some(11));
            sessions
        }
        other => panic!("expected SessionList, got {other:?}"),
    }
}

/// Poll `ListSessions` until `predicate` holds, or fail after two seconds.
async fn wait_for(
    connection: &mut Connection<UnixStream>,
    what: &str,
    predicate: impl Fn(&[SessionInfo]) -> bool,
) -> Vec<SessionInfo> {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let sessions = list(connection).await;
        if predicate(&sessions) {
            return sessions;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        // The server runs on the global executor's threads, so parking this
        // one is enough; `smol::Timer` is disallowed by the workspace lints.
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// How long any single frame may take to arrive before the test gives up.
/// Generous: a runner under full-workspace load can stall a frame for tens of
/// seconds, and a passing run never waits this long. Two of these can compose
/// in one wait (attach, then recv); the package's 120s slow-timeout override
/// in `.config/nextest.toml` keeps the labeled panic ahead of the harness
/// kill even then.
const FRAME_TIMEOUT: Duration = Duration::from_secs(45);

/// `connection.recv()` that fails the test instead of hanging forever.
///
/// The timeout is a blocking sleep moved onto smol's blocking pool rather than
/// a `smol::Timer`, which the workspace lints disallow.
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

fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    occurrences(haystack, needle) > 0
}

/// A `cat` with terminal echo off that announces itself once it is safe to
/// write to.
///
/// Echo off because these tests count occurrences: with echo on every byte
/// written comes back twice, once from the tty and once from `cat`. The
/// `READY` marker closes the race between `Created` and `stty` actually having
/// run â€” bytes written before that would still be echoed.
const CAT: &str = "sh -c 'stty -echo; printf READY; exec cat'";
const RAW_CAT: &str = "sh -c 'stty -echo -icanon min 1 time 0; printf READY; exec cat'";

/// Create a [`CAT`] session and wait until it is safe to write to it.
async fn cat_session(
    server: &RunningServer,
    connection: &mut Connection<UnixStream>,
    cwd: &Path,
) -> SessionInfo {
    let session = create(connection, cwd, CAT).await;
    wait_for_ring(server.socket_path(), &session.id, b"READY").await;
    session
}

async fn raw_cat_session(
    server: &RunningServer,
    connection: &mut Connection<UnixStream>,
    cwd: &Path,
) -> SessionInfo {
    let session = create(connection, cwd, RAW_CAT).await;
    wait_for_ring(server.socket_path(), &session.id, b"READY").await;
    session
}

/// Attach, asserting that the first frame back is the replay.
async fn attach(connection: &mut Connection<UnixStream>, id: &SessionId) -> (Vec<u8>, bool) {
    connection
        .send(&Frame::Attach {
            session_id: id.clone(),
            request_id: Some(21),
        })
        .await
        .expect("sending Attach");
    match recv(connection, "Replay").await {
        Frame::Replay {
            session_id,
            bytes,
            truncated,
        } => {
            assert_eq!(&session_id, id);
            (bytes, truncated)
        }
        other => panic!("expected Replay, got {other:?}"),
    }
}

async fn detach(connection: &mut Connection<UnixStream>, id: &SessionId) {
    connection
        .send(&Frame::Detach {
            session_id: id.clone(),
            request_id: None,
        })
        .await
        .expect("sending Detach");
}

async fn write_to(connection: &mut Connection<UnixStream>, id: &SessionId, bytes: &[u8]) {
    connection
        .send(&Frame::Write {
            session_id: id.clone(),
            bytes: bytes.to_vec(),
        })
        .await
        .expect("sending Write");
}

/// Read `Output` frames until the bytes seen so far contain `needle`.
async fn output_until(
    connection: &mut Connection<UnixStream>,
    id: &SessionId,
    needle: &[u8],
) -> Vec<u8> {
    let mut seen = Vec::new();
    while !contains(&seen, needle) {
        match recv(connection, "Output").await {
            Frame::Output { session_id, bytes } => {
                assert_eq!(&session_id, id);
                seen.extend_from_slice(&bytes);
            }
            other => panic!("expected Output, got {other:?}"),
        }
    }
    seen
}

/// Block until the session's replay contains `needle`, watching from a
/// throwaway connection so that the caller's own connection stays unattached.
///
/// The replay is a repaint of the session's screen, so `needle` has to be
/// something that ends up *on* the screen: a line ending is what moved the
/// cursor, not something painted at it.
///
/// Dropping that connection is also the implicit detach, which is exactly what
/// a client crash does.
///
/// On a miss the helper waits for one live `Output` frame and re-attaches:
/// attach and publish share the session lock, so an `Output` after the attach
/// proves the grid changed and a fresh replay is worth taking. Waiting for the
/// needle in the output itself would starve — a repaint (leaving the alternate
/// screen) puts bytes on the screen that never appear in raw output.
async fn wait_for_ring(socket: &Path, id: &SessionId, needle: &[u8]) {
    let deadline = Instant::now() + FRAME_TIMEOUT;
    loop {
        let mut probe = client(socket).await;
        let (replayed, _) = attach(&mut probe, id).await;
        if contains(&replayed, needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "replay never contained {needle:?}"
        );
        match recv(&mut probe, "Output").await {
            Frame::Output { session_id, .. } => assert_eq!(&session_id, id),
            other => panic!("expected Output, got {other:?}"),
        }
    }
}

async fn subscribe(connection: &mut Connection<UnixStream>) {
    connection
        .send(&Frame::Subscribe {
            request_id: Some(31),
        })
        .await
        .expect("sending Subscribe");
}

/// Read frames until `want` accepts one, ignoring everything else.
///
/// Ignoring is the point: a subscriber that is also attached sees `Output`
/// interleaved with its events, and every test here cares about one kind at a
/// time.
async fn event_until<T>(
    connection: &mut Connection<UnixStream>,
    what: &str,
    want: impl Fn(&Frame) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let frame = recv(connection, what).await;
        if let Some(found) = want(&frame) {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
    }
}

/// The next [`Frame::Status`] for `id`, whatever it says.
async fn next_status(connection: &mut Connection<UnixStream>, id: &SessionId) -> SessionStatus {
    event_until(connection, "a Status frame", |frame| match frame {
        Frame::Status {
            session_id, status, ..
        } if session_id == id => Some(*status),
        _ => None,
    })
    .await
}

/// Read `Status` frames for `id` until one reports `wanted`.
async fn status_becomes(
    connection: &mut Connection<UnixStream>,
    id: &SessionId,
    wanted: SessionStatus,
) -> u64 {
    let what = format!("status {wanted:?}");
    event_until(connection, &what, |frame| match frame {
        Frame::Status {
            session_id,
            status,
            since,
        } if session_id == id && *status == wanted => Some(*since),
        _ => None,
    })
    .await
}

/// Attach until the replay contains `needle`. Only sound for a session that
/// has stopped producing output, since it detaches and retries.
async fn replay_containing(
    connection: &mut Connection<UnixStream>,
    id: &SessionId,
    needle: &[u8],
) -> (Vec<u8>, bool) {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let (bytes, truncated) = attach(connection, id).await;
        if contains(&bytes, needle) {
            return (bytes, truncated);
        }
        detach(connection, id).await;
        assert!(
            Instant::now() < deadline,
            "timed out waiting for a replay containing {:?}",
            String::from_utf8_lossy(needle)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---- workspaces ----

/// `CreateWorkspace`, asserting the daemon answers with the whole thing.
async fn create_workspace(
    connection: &mut Connection<UnixStream>,
    root: &Path,
    name: Option<&str>,
) -> (WorkspaceInfo, Vec<SessionInfo>) {
    connection
        .send(&Frame::CreateWorkspace {
            root: root.display().to_string(),
            name: name.map(str::to_owned),
            env: vec![("ADE_TEST".into(), "1".into())],
            cols: Some(80),
            rows: Some(24),
            request_id: Some(61),
        })
        .await
        .expect("sending CreateWorkspace");
    // By correlation id, not by frame kind: a subscribed client is also told
    // about its own new workspace, ahead of its reply.
    event_until(
        connection,
        "the CreateWorkspace reply",
        |frame| match frame {
            Frame::Workspace {
                workspace,
                sessions,
                request_id: Some(61),
            } => Some((workspace.clone(), sessions.clone())),
            Frame::Error { message, .. } => panic!("CreateWorkspace failed: {message}"),
            _ => None,
        },
    )
    .await
}

async fn open_workspace(
    connection: &mut Connection<UnixStream>,
    id: &str,
) -> (WorkspaceInfo, Vec<SessionInfo>) {
    connection
        .send(&Frame::OpenWorkspace {
            workspace_id: id.to_owned(),
            request_id: Some(62),
        })
        .await
        .expect("sending OpenWorkspace");
    event_until(connection, "the OpenWorkspace reply", |frame| match frame {
        Frame::Workspace {
            workspace,
            sessions,
            request_id: Some(62),
        } => Some((workspace.clone(), sessions.clone())),
        Frame::Error { message, .. } => panic!("OpenWorkspace failed: {message}"),
        _ => None,
    })
    .await
}

async fn list_workspaces(connection: &mut Connection<UnixStream>) -> Vec<WorkspaceInfo> {
    connection
        .send(&Frame::ListWorkspaces {
            request_id: Some(63),
        })
        .await
        .expect("sending ListWorkspaces");
    event_until(
        connection,
        "the ListWorkspaces reply",
        |frame| match frame {
            Frame::WorkspaceList {
                workspaces,
                request_id: Some(63),
            } => Some(workspaces.clone()),
            _ => None,
        },
    )
    .await
}

/// The reply frame, whatever it is: a rename is refused as often as it is
/// accepted, and both answers are this test's business.
async fn rename_workspace(connection: &mut Connection<UnixStream>, id: &str, name: &str) -> Frame {
    connection
        .send(&Frame::RenameWorkspace {
            workspace_id: id.to_owned(),
            name: name.to_owned(),
            request_id: Some(67),
        })
        .await
        .expect("sending RenameWorkspace");
    event_until(
        connection,
        "the RenameWorkspace reply",
        |frame| match frame {
            Frame::Workspace {
                request_id: Some(67),
                ..
            }
            | Frame::Error {
                request_id: Some(67),
                ..
            } => Some(frame.clone()),
            _ => None,
        },
    )
    .await
}

/// The reply frame, whatever it is: these tests assert on rejections as much
/// as on acceptances.
async fn update_layout(
    connection: &mut Connection<UnixStream>,
    id: &str,
    layout: LayoutDoc,
    rev: u64,
) -> Frame {
    connection
        .send(&Frame::UpdateLayout {
            workspace_id: id.to_owned(),
            layout,
            rev,
            request_id: Some(64),
        })
        .await
        .expect("sending UpdateLayout");
    event_until(connection, "the UpdateLayout reply", |frame| match frame {
        Frame::LayoutChanged { .. } | Frame::Error { .. } => Some(frame.clone()),
        _ => None,
    })
    .await
}

fn terminal_leaf(id: &SessionId) -> LayoutNode {
    LayoutNode::leaf(vec![Tab::Terminal {
        session_id: id.clone(),
    }])
}

fn editor_leaf(path: &str) -> LayoutNode {
    LayoutNode::leaf(vec![Tab::Editor {
        path: path.to_owned(),
    }])
}

/// Two terminals side by side: a document one tab can be scrubbed out of
/// without taking its sibling with it.
fn two_terminals(first: &SessionId, second: &SessionId) -> LayoutDoc {
    LayoutDoc::new(LayoutNode::Split {
        dir: SplitDir::Horizontal,
        ratio: 0.5,
        children: Box::new([terminal_leaf(first), terminal_leaf(second)]),
    })
}

/// A terminal beside an editor: the terminal half dies with the daemon, the
/// editor half does not.
fn terminal_beside_editor(id: &SessionId, path: &str) -> LayoutDoc {
    LayoutDoc::new(LayoutNode::Split {
        dir: SplitDir::Horizontal,
        ratio: 0.5,
        children: Box::new([terminal_leaf(id), editor_leaf(path)]),
    })
}

/// Killing a session takes its tab out of the workspace's layout, in the same
/// operation.
///
/// The tab and the session go together â€” tmux's `kill-pane`. Without it the
/// stored layout keeps naming a session that no longer exists and the workspace
/// wedges: the client's next `UpdateLayout` names it too and is refused.
#[test]
fn killing_a_session_scrubs_its_tab_from_the_layout() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        // Both in one workspace: `create` puts every session in `ws-1`, which
        // the daemon mints on the first one.
        let first = create(&mut connection, dir.path(), CAT).await.id;
        let second = create(&mut connection, dir.path(), CAT).await.id;
        let (workspace, _) = open_workspace(&mut connection, "ws-1").await;

        // Two tabs in one leaf, the second of them active.
        let layout = LayoutDoc::new(LayoutNode::Leaf {
            tabs: vec![
                Tab::Terminal {
                    session_id: first.clone(),
                },
                Tab::Terminal {
                    session_id: second.clone(),
                },
            ],
            active: 1,
            focused: false,
        });
        update_layout(
            &mut connection,
            &workspace.id,
            layout,
            workspace.layout_rev + 1,
        )
        .await;

        kill(&mut connection, &second).await;

        let (after, _) = open_workspace(&mut connection, &workspace.id).await;
        assert_eq!(
            after.layout.terminal_sessions(),
            vec![first],
            "the killed tab is gone and the other one stayed"
        );
        assert_eq!(
            after.layout_rev,
            workspace.layout_rev + 2,
            "the scrub is a real change to a stored document, so the rev moves"
        );
        // `active` pointed at the tab that just went; it has to be brought back
        // into range or the client indexes past the end of its own tab strip.
        match &after.layout.root {
            LayoutNode::Leaf { active, tabs, .. } => {
                assert_eq!(*active, 0, "active clamped into the surviving tabs");
                assert_eq!(tabs.len(), 1);
            }
            other => panic!("expected a leaf, got {other:?}"),
        }
    });
}

/// A split that loses a child becomes its sibling, and an editor beside it is
/// untouched.
#[test]
fn killing_the_last_terminal_in_a_split_collapses_it_onto_the_sibling() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let (workspace, sessions) = create_workspace(&mut connection, dir.path(), None).await;
        let session = sessions[0].id.clone();

        let layout = terminal_beside_editor(&session, "src/main.rs");
        update_layout(
            &mut connection,
            &workspace.id,
            layout,
            workspace.layout_rev + 1,
        )
        .await;

        kill(&mut connection, &session).await;

        let (after, _) = open_workspace(&mut connection, &workspace.id).await;
        assert!(after.layout.terminal_sessions().is_empty());
        match &after.layout.root {
            // The split is gone entirely: what is left is the editor leaf that
            // was its other child, promoted in place of it.
            LayoutNode::Leaf { tabs, .. } => assert_eq!(
                tabs,
                &vec![Tab::Editor {
                    path: "src/main.rs".to_owned()
                }],
                "the editor half survives its terminal neighbour"
            ),
            other => panic!("expected the split to collapse onto the editor, got {other:?}"),
        }
    });
}

/// The scrub goes to **every** subscriber, the killer included.
///
/// `UpdateLayout` excludes the writer because that client already holds the
/// document it sent. Here the daemon decided, so nobody holds it â€” and the
/// killer needs it most, since its own next `UpdateLayout` would otherwise be
/// built on a layout naming the session it just killed.
#[test]
fn the_scrub_is_broadcast_to_the_killer_too() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut killer = client(server.socket_path()).await;
        let (workspace, sessions) = create_workspace(&mut killer, dir.path(), None).await;
        let session = sessions[0].id.clone();
        subscribe(&mut killer).await;

        let mut observer = client(server.socket_path()).await;
        subscribe(&mut observer).await;

        kill(&mut killer, &session).await;

        for (who, connection) in [("the killer", &mut killer), ("an observer", &mut observer)] {
            let (id, rev) = event_until(connection, "LayoutChanged", |frame| match frame {
                Frame::LayoutChanged {
                    workspace_id,
                    rev,
                    layout,
                    ..
                } => Some((workspace_id.clone(), (*rev, layout.terminal_sessions()))),
                _ => None,
            })
            .await;
            assert_eq!(id, workspace.id, "{who}");
            assert_eq!(rev.0, workspace.layout_rev + 1, "{who}: the rev moved once");
            assert!(rev.1.is_empty(), "{who}: the tab is gone from the document");
        }
    });
}

/// A client that killed a session and then pushes a layout still naming it is
/// refused â€” but the workspace is not wedged, because the scrub already told it
/// what the document is now.
///
/// This is the race the scrub exists for: Zed emits `ItemRemoved` for a tab
/// that was only *dragged* into a split, so the `Kill` and the stale
/// `UpdateLayout` are both in flight.
#[test]
fn a_layout_update_naming_a_killed_session_is_refused_and_the_scrub_stands() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let (workspace, sessions) = create_workspace(&mut connection, dir.path(), None).await;
        let session = sessions[0].id.clone();

        kill(&mut connection, &session).await;

        // The client's stale view, built before it learned about the scrub.
        let stale = LayoutDoc::single_terminal(session.clone());
        match update_layout(
            &mut connection,
            &workspace.id,
            stale,
            workspace.layout_rev + 2,
        )
        .await
        {
            Frame::Error { code, message, .. } => {
                assert_eq!(code, error_code::INVALID_ARGUMENT);
                assert!(
                    message.contains("unknown session"),
                    "refused for naming a session that is gone, got {message:?}"
                );
            }
            other => panic!("expected the stale update to be refused, got {other:?}"),
        }

        let (after, _) = open_workspace(&mut connection, &workspace.id).await;
        assert!(
            after.layout.terminal_sessions().is_empty(),
            "the daemon's own scrub is what stands: {:?}",
            after.layout
        );
        assert_eq!(after.layout_rev, workspace.layout_rev + 1);
    });
}

/// Run two client actions at once, each on its own thread and its own
/// connection, so that the daemon really does see them overlap.
///
/// The barrier is passed in rather than waited on here: each half connects and
/// handshakes first, then meets at the gate, so the two frames go out within
/// microseconds of each other instead of a connect apart. Nothing forces a
/// particular interleaving — the point of every test below is that *no*
/// interleaving may leave the table inconsistent.
fn race(
    socket: &Path,
    left: impl FnOnce(PathBuf, Arc<Barrier>) + Send + 'static,
    right: impl FnOnce(PathBuf, Arc<Barrier>) + Send + 'static,
) {
    let gate = Arc::new(Barrier::new(2));
    let (left_socket, right_socket) = (socket.to_owned(), socket.to_owned());
    let (left_gate, right_gate) = (gate.clone(), gate);
    let left = std::thread::spawn(move || left(left_socket, left_gate));
    let right = std::thread::spawn(move || right(right_socket, right_gate));
    left.join().expect("the first racer");
    right.join().expect("the second racer");
}

/// One half of a [`race`]: connect, wait at the gate, kill.
fn kill_racer(victim: SessionId) -> impl FnOnce(PathBuf, Arc<Barrier>) + Send + 'static {
    move |socket, gate| {
        smol::block_on(async move {
            let mut connection = client(&socket).await;
            gate.wait();
            kill(&mut connection, &victim).await;
        });
    }
}

/// Concurrent writers may race, but revisions, observer events and the ledger
/// all agree on the same winner. A higher revision either follows the lower
/// one or makes it stale; it can never be overwritten by it.
#[test]
fn concurrent_layout_writers_keep_the_highest_revision_across_restart() {
    let dir = TempDir::new().expect("temp dir");
    let first = server_named(&dir, "first.sock");
    let socket = first.socket_path().to_owned();
    let (workspace, high_layout) = smol::block_on(async {
        let mut owner = client(&socket).await;
        let (workspace, _) = create_workspace(&mut owner, dir.path(), None).await;
        let mut observer = client(&socket).await;
        subscribe(&mut observer).await;

        let low_layout = LayoutDoc::new(editor_leaf("low.rs"));
        let high_layout = LayoutDoc::new(editor_leaf("high.rs"));
        let low_id = workspace.id.clone();
        let high_id = workspace.id.clone();
        let low = low_layout.clone();
        let high = high_layout.clone();
        race(
            &socket,
            move |socket, gate| {
                smol::block_on(async move {
                    let mut writer = client(&socket).await;
                    gate.wait();
                    update_layout(&mut writer, &low_id, low, 2).await;
                });
            },
            move |socket, gate| {
                smol::block_on(async move {
                    let mut writer = client(&socket).await;
                    gate.wait();
                    update_layout(&mut writer, &high_id, high, 3).await;
                });
            },
        );

        // A request on the observer connection is a FIFO fence behind every
        // layout event that the completed writers already queued for it.
        observer
            .send(&Frame::ListWorkspaces {
                request_id: Some(63),
            })
            .await
            .expect("sending the event fence");
        let mut revisions = Vec::new();
        loop {
            match recv(&mut observer, "layout events and fence").await {
                Frame::LayoutChanged {
                    workspace_id, rev, ..
                } if workspace_id == workspace.id => revisions.push(rev),
                Frame::WorkspaceList {
                    request_id: Some(63),
                    ..
                } => break,
                _ => {}
            }
        }
        assert!(
            revisions.windows(2).all(|pair| pair[0] < pair[1]),
            "layout events went backwards: {revisions:?}"
        );
        assert_eq!(revisions.last(), Some(&3), "{revisions:?}");

        let (stored, _) = open_workspace(&mut owner, &workspace.id).await;
        assert_eq!(stored.layout_rev, 3);
        assert_eq!(stored.layout, high_layout);
        (workspace, high_layout)
    });
    drop(first);

    let second = server_named(&dir, "second.sock");
    smol::block_on(async {
        let mut connection = client(second.socket_path()).await;
        let (restored, _) = open_workspace(&mut connection, &workspace.id).await;
        assert_eq!(restored.layout_rev, 3);
        assert_eq!(restored.layout, high_layout);
    });
}

/// A `Kill` and an `UpdateLayout` for the same workspace, in flight at once.
///
/// Forgetting the row and pruning its tab are one daemon transaction, so there
/// is no instant between them for the update to land in. Either order is fine:
/// the update wins and is scrubbed, or it is refused as stale because the
/// scrub took the document to the rev it was aiming at. A stored layout naming
/// a session the daemon no longer has is not fine, in either order.
#[test]
fn a_kill_racing_a_layout_update_leaves_no_tab_naming_a_dead_session() {
    let (dir, server) = server();
    let socket = server.socket_path().to_owned();
    let (workspace_id, survivor, victim) = smol::block_on(async {
        let mut connection = client(&socket).await;
        let survivor = create(&mut connection, dir.path(), "sleep 300").await.id;
        let victim = create(&mut connection, dir.path(), "sleep 300").await.id;
        // `create` puts every session in `ws-1`, which the daemon minted at
        // rev 1 on the first one; this takes it to rev 2 with both tabs in it.
        let (workspace, _) = open_workspace(&mut connection, "ws-1").await;
        update_layout(
            &mut connection,
            &workspace.id,
            two_terminals(&survivor, &victim),
            workspace.layout_rev + 1,
        )
        .await;
        (workspace.id, survivor, victim)
    });

    let racing_id = workspace_id.clone();
    let stale = two_terminals(&survivor, &victim);
    race(&socket, kill_racer(victim.clone()), move |socket, gate| {
        smol::block_on(async move {
            let mut connection = client(&socket).await;
            gate.wait();
            // Rev 3: what a client holding the rev-2 document would send,
            // and what the scrub itself moves the document to.
            update_layout(&mut connection, &racing_id, stale, 3).await;
        });
    });

    smol::block_on(async {
        let mut connection = client(&socket).await;
        let alive: Vec<SessionId> = list(&mut connection)
            .await
            .into_iter()
            .map(|session| session.id)
            .collect();
        assert_eq!(alive, vec![survivor.clone()], "the kill landed");
        let (after, _) = open_workspace(&mut connection, &workspace_id).await;
        assert_eq!(
            after.layout.terminal_sessions(),
            vec![survivor],
            "the killed tab is gone whichever way the race went: {:?}",
            after.layout
        );
        assert!(
            !after.layout.terminal_sessions().contains(&victim),
            "the layout names a session the daemon does not have"
        );
    });
}

/// Two kills at once, each ending in its own `persist`.
///
/// The write is serialized end to end, so the file left on disk cannot be the
/// older of the two snapshots — one still naming the session the other kill
/// removed, which the next daemon would report as a lost row.
#[test]
fn two_concurrent_kills_stay_killed_across_a_restart() {
    let dir = TempDir::new().expect("temp dir");
    let first = server_named(&dir, "first.sock");
    let socket = first.socket_path().to_owned();
    let (left, right, survivor) = smol::block_on(async {
        let mut connection = client(&socket).await;
        let left = create(&mut connection, dir.path(), "sleep 300").await.id;
        let right = create(&mut connection, dir.path(), "sleep 300").await.id;
        let survivor = create(&mut connection, dir.path(), "sleep 300").await.id;
        (left, right, survivor)
    });

    race(&socket, kill_racer(left.clone()), kill_racer(right.clone()));

    smol::block_on(async {
        let mut connection = client(&socket).await;
        let alive: Vec<SessionId> = list(&mut connection)
            .await
            .into_iter()
            .map(|session| session.id)
            .collect();
        assert_eq!(alive, vec![survivor.clone()], "both kills landed");
    });
    drop(first);

    let second = server_named(&dir, "second.sock");
    smol::block_on(async {
        let mut connection = client(second.socket_path()).await;
        let sessions = list(&mut connection).await;
        assert_eq!(
            sessions.len(),
            1,
            "a killed session came back: {sessions:?}"
        );
        assert_eq!(sessions[0].id, survivor);
        let workspaces = list_workspaces(&mut connection).await;
        assert_eq!(workspaces.len(), 1, "{workspaces:?}");
        let tabs = workspaces[0].layout.terminal_sessions();
        assert!(
            !tabs.contains(&left) && !tabs.contains(&right),
            "a killed session's tab came back: {tabs:?}"
        );
    });
}

/// `KillWorkspace` racing a `CreateSession` naming the same workspace.
///
/// Naming the doomed sessions and dropping the record are one transaction, so
/// the create is either wholly before the kill — and its session goes with the
/// workspace — or wholly after, and the daemon records the workspace again for
/// it. What can never be left behind is a session claiming a workspace the
/// daemon has announced as removed.
#[test]
fn killing_a_workspace_while_a_session_is_created_leaves_no_orphaned_session() {
    let (dir, server) = server();
    let socket = server.socket_path().to_owned();
    smol::block_on(async {
        let mut connection = client(&socket).await;
        create(&mut connection, dir.path(), "sleep 300").await;
    });

    let cwd = dir.path().to_owned();
    race(
        &socket,
        move |socket, gate| {
            smol::block_on(async move {
                let mut connection = client(&socket).await;
                gate.wait();
                connection
                    .send(&Frame::KillWorkspace {
                        workspace_id: "ws-1".to_owned(),
                        request_id: Some(65),
                    })
                    .await
                    .expect("sending KillWorkspace");
                event_until(
                    &mut connection,
                    "the KillWorkspace reply",
                    |frame| match frame {
                        Frame::WorkspaceRemoved {
                            request_id: Some(65),
                            ..
                        }
                        | Frame::Error {
                            request_id: Some(65),
                            ..
                        } => Some(()),
                        _ => None,
                    },
                )
                .await;
            });
        },
        move |socket, gate| {
            smol::block_on(async move {
                let mut connection = client(&socket).await;
                gate.wait();
                create(&mut connection, &cwd, "sleep 300").await;
            });
        },
    );

    smol::block_on(async {
        let mut connection = client(&socket).await;
        let claimants: Vec<SessionId> = list(&mut connection)
            .await
            .into_iter()
            .filter(|session| session.workspace_id == "ws-1")
            .map(|session| session.id)
            .collect();
        let recorded = list_workspaces(&mut connection)
            .await
            .iter()
            .any(|workspace| workspace.id == "ws-1");
        assert!(
            claimants.is_empty() || recorded,
            "sessions {claimants:?} claim a workspace that was announced removed"
        );
    });
}

/// A killed session is gone for good: the file it was written out of names
/// neither it nor its tab, so the next daemon cannot report it as lost.
#[test]
fn a_killed_session_and_its_tab_do_not_survive_a_restart() {
    let dir = TempDir::new().expect("temp dir");
    let first = server_named(&dir, "first.sock");
    let (workspace_id, survivor) = smol::block_on(async {
        let mut connection = client(first.socket_path()).await;
        let survivor = create(&mut connection, dir.path(), "sleep 300").await.id;
        let victim = create(&mut connection, dir.path(), "sleep 300").await.id;
        let (workspace, _) = open_workspace(&mut connection, "ws-1").await;
        update_layout(
            &mut connection,
            &workspace.id,
            two_terminals(&survivor, &victim),
            workspace.layout_rev + 1,
        )
        .await;
        kill(&mut connection, &victim).await;
        (workspace.id, survivor)
    });
    drop(first);

    let second = server_named(&dir, "second.sock");
    smol::block_on(async {
        let mut connection = client(second.socket_path()).await;
        let sessions = list(&mut connection).await;
        assert_eq!(
            sessions.len(),
            1,
            "the killed session came back: {sessions:?}"
        );
        assert_eq!(sessions[0].id, survivor);
        // The survivor is a lost row, so its tab is still honest and stays;
        // the killed one was scrubbed before the write and has nothing left.
        let (restored, _) = open_workspace(&mut connection, &workspace_id).await;
        assert_eq!(
            restored.layout.terminal_sessions(),
            vec![survivor],
            "only the surviving tab: {:?}",
            restored.layout
        );
    });
}

/// `KillWorkspace` pops the record before killing its sessions, so there is no
/// layout left to scrub and no `LayoutChanged` for a workspace that is gone.
#[test]
fn killing_a_workspace_announces_its_removal_and_not_a_layout_change() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let (workspace, _) = create_workspace(&mut connection, dir.path(), None).await;
        subscribe(&mut connection).await;

        connection
            .send(&Frame::KillWorkspace {
                workspace_id: workspace.id.clone(),
                request_id: Some(65),
            })
            .await
            .expect("sending KillWorkspace");

        // The first layout-or-removal event about this workspace must be the
        // removal: a `LayoutChanged` here would describe a workspace that no
        // longer exists.
        let removed = event_until(&mut connection, "WorkspaceRemoved", |frame| match frame {
            Frame::WorkspaceRemoved { workspace_id, .. } if *workspace_id == workspace.id => {
                Some(true)
            }
            Frame::LayoutChanged { workspace_id, .. } if *workspace_id == workspace.id => {
                Some(false)
            }
            _ => None,
        })
        .await;
        assert!(removed, "WorkspaceRemoved, not LayoutChanged");
    });
}

/// The whole gesture in one round trip: a workspace, its login shell, and a
/// layout holding that shell's terminal tab.
#[test]
fn creating_a_workspace_returns_its_session_and_a_one_leaf_layout() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let (workspace, sessions) = create_workspace(&mut connection, dir.path(), None).await;

        assert_eq!(sessions.len(), 1, "{sessions:?}");
        let session = &sessions[0];
        assert_eq!(session.workspace_id, workspace.id);
        assert_eq!(session.cwd, dir.path().display().to_string());
        assert_eq!(workspace.project_root, dir.path().display().to_string());
        assert_eq!(workspace.layout_rev, 1);
        assert_eq!(
            workspace.layout.terminal_sessions(),
            vec![session.id.clone()]
        );
        assert_eq!(workspace.layout.root, terminal_leaf(&session.id));
        // The name defaults to the last component of the root.
        assert_eq!(
            workspace.name,
            dir.path().file_name().unwrap().to_string_lossy()
        );

        // And the workspace is listed, alone.
        let listed = list_workspaces(&mut connection).await;
        assert_eq!(listed, vec![workspace]);
    });
}

#[test]
fn a_blank_workspace_name_defaults_consistently() {
    let dir = TempDir::new().expect("temp dir");
    let first = server_named(&dir, "first.sock");
    let workspace = smol::block_on(async {
        let mut connection = client(first.socket_path()).await;
        let (workspace, _) = create_workspace(&mut connection, dir.path(), Some(" \t ")).await;
        assert_eq!(
            workspace.name,
            dir.path()
                .file_name()
                .expect("temp dir name")
                .to_string_lossy()
        );
        assert_eq!(
            open_workspace(&mut connection, &workspace.id).await.0,
            workspace
        );
        assert_eq!(
            list_workspaces(&mut connection).await,
            vec![workspace.clone()]
        );
        workspace
    });
    drop(first);

    let second = server_named(&dir, "second.sock");
    smol::block_on(async {
        let mut connection = client(second.socket_path()).await;
        assert_eq!(list_workspaces(&mut connection).await, vec![workspace]);
    });
}

/// Opening returns the layout plus every session in the workspace â€” including
/// the ones added afterwards, since a `+` in a tab bar is a plain
/// `CreateSession` carrying the workspace id.
#[test]
fn opening_a_workspace_returns_its_layout_and_every_session_in_it() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let (workspace, created) =
            create_workspace(&mut connection, dir.path(), Some("proj")).await;
        assert_eq!(workspace.name, "proj");

        connection
            .send(&Frame::CreateSession {
                workspace_id: workspace.id.clone(),
                cwd: dir.path().display().to_string(),
                command: "sleep 300".into(),
                env: Vec::new(),
                cols: 80,
                rows: 24,
                agent_kind: "shell".into(),
                instance_label: "second".into(),
                scrollback_bytes: None,
                request_id: Some(65),
            })
            .await
            .expect("sending CreateSession");
        let second = match recv(&mut connection, "Created").await {
            Frame::Created { session, .. } => session,
            other => panic!("expected Created, got {other:?}"),
        };

        let (opened, sessions) = open_workspace(&mut connection, &workspace.id).await;
        assert_eq!(opened, workspace);
        // Sorted on both sides: sessions created inside the same second are
        // ordered by id, which is a uuid.
        let mut got: Vec<SessionId> = sessions.iter().map(|s| s.id.clone()).collect();
        got.sort();
        let mut want = vec![created[0].id.clone(), second.id.clone()];
        want.sort();
        assert_eq!(got, want);

        // An id nobody created is an error, not an empty workspace.
        connection
            .send(&Frame::OpenWorkspace {
                workspace_id: "nope".into(),
                request_id: Some(62),
            })
            .await
            .expect("sending OpenWorkspace");
        match recv(&mut connection, "Error").await {
            Frame::Error {
                code,
                message,
                workspace_id,
                ..
            } => {
                assert_eq!(code, error_code::NOT_FOUND);
                assert_eq!(workspace_id.as_deref(), Some("nope"));
                assert!(message.contains("nope"), "{message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    });
}

/// The revision guard: a client writing from a stale view is rejected and the
/// stored layout stands.
#[test]
fn a_stale_layout_rev_is_rejected_and_the_stored_layout_stands() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let (workspace, sessions) = create_workspace(&mut connection, dir.path(), None).await;
        let session = sessions[0].id.clone();

        let wanted = terminal_beside_editor(&session, "/src/main.rs");
        match update_layout(&mut connection, &workspace.id, wanted.clone(), 2).await {
            Frame::LayoutChanged {
                layout,
                rev,
                request_id,
                ..
            } => {
                assert_eq!(layout, wanted);
                assert_eq!(rev, 2);
                assert_eq!(request_id, Some(64));
            }
            other => panic!("expected LayoutChanged, got {other:?}"),
        }

        // Both a rev below and a rev equal to the stored one are stale.
        for stale in [1, 2] {
            match update_layout(
                &mut connection,
                &workspace.id,
                LayoutDoc::single_terminal(session.clone()),
                stale,
            )
            .await
            {
                Frame::Error {
                    code,
                    message,
                    workspace_id,
                    ..
                } => {
                    assert_eq!(code, error_code::STALE_REV);
                    assert_eq!(workspace_id.as_deref(), Some(workspace.id.as_str()));
                    assert!(message.contains("stale"), "{message}");
                }
                other => panic!("expected Error for rev {stale}, got {other:?}"),
            }
        }

        let (reopened, _) = open_workspace(&mut connection, &workspace.id).await;
        assert_eq!(reopened.layout, wanted);
        assert_eq!(reopened.layout_rev, 2);
    });
}

/// The one thing the daemon validates inside a layout: a terminal tab must
/// name a session it owns. Editor tabs are opaque and never checked.
#[test]
fn a_layout_naming_an_unknown_session_is_rejected() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let (workspace, _) = create_workspace(&mut connection, dir.path(), None).await;

        let bogus = LayoutDoc::single_terminal(SessionId::new("not-a-session"));
        match update_layout(&mut connection, &workspace.id, bogus, 2).await {
            Frame::Error { code, message, .. } => {
                // Not `not_found`: the workspace is there and the revision is
                // fine, it is the document the client sent that is wrong.
                assert_eq!(code, error_code::INVALID_ARGUMENT);
                assert!(message.contains("not-a-session"), "{message}")
            }
            other => panic!("expected Error, got {other:?}"),
        }

        // The same document with only an editor tab is accepted: what an editor
        // tab points at is the client's business.
        let editors = LayoutDoc::new(editor_leaf("/src/main.rs"));
        match update_layout(&mut connection, &workspace.id, editors.clone(), 2).await {
            Frame::LayoutChanged { layout, .. } => assert_eq!(layout, editors),
            other => panic!("expected LayoutChanged, got {other:?}"),
        }
    });
}

/// A terminal belongs to exactly one workspace. Merely existing in the daemon
/// is not enough: otherwise killing it scrubs its owner while a second
/// workspace keeps a dead terminal id forever.
#[test]
fn a_layout_cannot_borrow_a_session_from_another_workspace() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let (first, _) = create_workspace(&mut connection, dir.path(), Some("first")).await;
        let (_, second_sessions) =
            create_workspace(&mut connection, dir.path(), Some("second")).await;
        let foreign = second_sessions[0].id.clone();

        match update_layout(
            &mut connection,
            &first.id,
            LayoutDoc::single_terminal(foreign.clone()),
            first.layout_rev + 1,
        )
        .await
        {
            Frame::Error { code, message, .. } => {
                assert_eq!(code, error_code::INVALID_ARGUMENT);
                assert!(message.contains("another workspace"), "{message}");
            }
            other => panic!("expected the foreign session to be refused, got {other:?}"),
        }

        let (unchanged, _) = open_workspace(&mut connection, &first.id).await;
        assert_eq!(unchanged.layout, first.layout);
        assert!(!unchanged.layout.terminal_sessions().contains(&foreign));
    });
}

/// A layout change reaches the *other* attached clients, on the same push
/// channel as status events. The writer gets its own reply and no echo.
#[test]
fn an_accepted_layout_is_pushed_to_the_other_clients() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut writer = client(server.socket_path()).await;
        let mut observer = client(server.socket_path()).await;
        subscribe(&mut writer).await;
        subscribe(&mut observer).await;
        let (workspace, sessions) = create_workspace(&mut writer, dir.path(), None).await;

        let wanted = terminal_beside_editor(&sessions[0].id, "/src/lib.rs");
        // The broadcast is queued before the reply, so an unfiltered fan-out
        // would show up here as a `LayoutChanged` with no request id.
        match update_layout(&mut writer, &workspace.id, wanted.clone(), 2).await {
            Frame::LayoutChanged { request_id, .. } => assert_eq!(request_id, Some(64)),
            other => panic!("expected LayoutChanged, got {other:?}"),
        }

        let (layout, rev, request_id) =
            event_until(&mut observer, "LayoutChanged", |frame| match frame {
                Frame::LayoutChanged {
                    workspace_id,
                    layout,
                    rev,
                    request_id,
                } if *workspace_id == workspace.id => Some((layout.clone(), *rev, *request_id)),
                _ => None,
            })
            .await;
        assert_eq!(layout, wanted);
        assert_eq!(rev, 2);
        assert_eq!(request_id, None, "a broadcast carries no correlation id");
    });
}

/// The only workspace-level kill there is: it takes the sessions with it.
#[test]
fn killing_a_workspace_kills_every_session_in_it() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let mut observer = client(server.socket_path()).await;
        subscribe(&mut observer).await;
        let (workspace, sessions) = create_workspace(&mut connection, dir.path(), None).await;
        let session = sessions[0].id.clone();

        connection
            .send(&Frame::KillWorkspace {
                workspace_id: workspace.id.clone(),
                request_id: Some(66),
            })
            .await
            .expect("sending KillWorkspace");
        match recv(&mut connection, "WorkspaceRemoved").await {
            Frame::WorkspaceRemoved {
                workspace_id,
                request_id,
            } => {
                assert_eq!(workspace_id, workspace.id);
                assert_eq!(request_id, Some(66));
            }
            other => panic!("expected WorkspaceRemoved, got {other:?}"),
        }

        // The other clients learn about it on the event stream: every session
        // removed, then the workspace itself.
        let removed = event_until(&mut observer, "Removed", |frame| match frame {
            Frame::Removed { session_id } => Some(session_id.clone()),
            Frame::WorkspaceRemoved { .. } => panic!("the workspace went before its session"),
            _ => None,
        })
        .await;
        assert_eq!(removed, session);
        event_until(&mut observer, "WorkspaceRemoved", |frame| match frame {
            Frame::WorkspaceRemoved { workspace_id, .. } if *workspace_id == workspace.id => {
                Some(())
            }
            _ => None,
        })
        .await;

        assert!(list_workspaces(&mut connection).await.is_empty());
        assert!(list(&mut connection).await.is_empty());

        // Killing it twice is an error, not a second removal.
        connection
            .send(&Frame::KillWorkspace {
                workspace_id: workspace.id.clone(),
                request_id: Some(66),
            })
            .await
            .expect("sending KillWorkspace");
        match recv(&mut connection, "Error").await {
            Frame::Error { code, message, .. } => {
                assert_eq!(code, error_code::NOT_FOUND);
                assert!(message.contains(&workspace.id), "{message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    });
}

/// A name is display metadata; the id is identity. Renaming moves the one and
/// never the other, so the sessions, the layout and everything keyed by the id
/// stay exactly where they were â€” and the new name outlives the daemon.
#[test]
fn renaming_a_workspace_keeps_its_id_and_everything_under_it() {
    let dir = TempDir::new().expect("temp dir");
    let first = server_named(&dir, "first.sock");
    let workspace = smol::block_on(async {
        let mut connection = client(first.socket_path()).await;
        let mut observer = client(first.socket_path()).await;
        subscribe(&mut observer).await;
        let (workspace, sessions) =
            create_workspace(&mut connection, dir.path(), Some("proj")).await;

        // Surrounding whitespace is not part of a name.
        let (renamed, renamed_sessions) =
            match rename_workspace(&mut connection, &workspace.id, "  vector db spike  ").await {
                Frame::Workspace {
                    workspace,
                    sessions,
                    ..
                } => (workspace, sessions),
                other => panic!("expected Workspace, got {other:?}"),
            };
        assert_eq!(renamed.id, workspace.id, "the id must never move");
        assert_eq!(renamed.name, "vector db spike");
        assert_eq!(renamed.created_at, workspace.created_at);
        assert_eq!(renamed.layout, workspace.layout);
        assert_eq!(renamed.layout_rev, workspace.layout_rev);
        assert_eq!(
            renamed_sessions
                .iter()
                .map(|session| session.id.clone())
                .collect::<Vec<_>>(),
            sessions
                .iter()
                .map(|session| session.id.clone())
                .collect::<Vec<_>>(),
            "renaming must not disturb the sessions"
        );

        // Everyone watching is told, the same way a new workspace announces
        // itself.
        let announced = event_until(
            &mut observer,
            "the renamed workspace",
            |frame| match frame {
                Frame::Workspace {
                    workspace: announced,
                    request_id: None,
                    ..
                } if announced.name == "vector db spike" => Some(announced.clone()),
                _ => None,
            },
        )
        .await;
        assert_eq!(announced.id, workspace.id);

        // A workspace with no name is a row the user cannot tell from another.
        match rename_workspace(&mut connection, &workspace.id, "   ").await {
            Frame::Error { code, message, .. } => {
                assert_eq!(code, error_code::INVALID_ARGUMENT);
                assert!(message.contains("empty"), "{message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        // And a rename is never a creation.
        match rename_workspace(&mut connection, "no-such-workspace", "whatever").await {
            Frame::Error { code, message, .. } => {
                assert_eq!(code, error_code::NOT_FOUND);
                assert!(message.contains("no-such-workspace"), "{message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert_eq!(list_workspaces(&mut connection).await.len(), 1);

        // Neither refusal touched the accepted name.
        let (opened, _) = open_workspace(&mut connection, &workspace.id).await;
        assert_eq!(opened.name, "vector db spike");
        workspace
    });
    drop(first);

    let second = server_named(&dir, "second.sock");
    smol::block_on(async {
        let mut connection = client(second.socket_path()).await;
        let workspaces = list_workspaces(&mut connection).await;
        assert_eq!(workspaces.len(), 1, "{workspaces:?}");
        assert_eq!(workspaces[0].id, workspace.id);
        assert_eq!(workspaces[0].name, "vector db spike");
    });
}

/// Workspaces are the durable half of the daemon: name, root, layout and
/// `layout_rev` all come back. The terminal tabs inside them are pruned only
/// once the sessions they name are gone for good.
#[test]
fn workspaces_and_their_layout_survive_a_restart() {
    let dir = TempDir::new().expect("temp dir");
    let first = server_named(&dir, "first.sock");
    let (workspace, session) = smol::block_on(async {
        let mut connection = client(first.socket_path()).await;
        let (workspace, sessions) =
            create_workspace(&mut connection, dir.path(), Some("proj")).await;
        let layout = terminal_beside_editor(&sessions[0].id, "/src/main.rs");
        match update_layout(&mut connection, &workspace.id, layout, 3).await {
            Frame::LayoutChanged { .. } => {}
            other => panic!("expected LayoutChanged, got {other:?}"),
        }
        (workspace, sessions[0].id.clone())
    });
    drop(first);

    let second = server_named(&dir, "second.sock");
    smol::block_on(async {
        let mut connection = client(second.socket_path()).await;
        let workspaces = list_workspaces(&mut connection).await;
        assert_eq!(workspaces.len(), 1, "{workspaces:?}");
        let restored = &workspaces[0];
        assert_eq!(restored.id, workspace.id);
        assert_eq!(restored.name, "proj");
        assert_eq!(restored.project_root, workspace.project_root);
        assert_eq!(restored.layout_rev, 3);
        // The session is lost but still listed, so its tab is still honest.
        assert_eq!(restored.layout.terminal_sessions(), vec![session.clone()]);
        assert_eq!(
            status_of(&list(&mut connection).await, &session),
            SessionStatus::Exited
        );
    });
    drop(second);

    // Once the lost row has been reported and forgotten, the tab naming it goes
    // too â€” and the editor half of the split survives it.
    let third = server_named(&dir, "third.sock");
    smol::block_on(async {
        let mut connection = client(third.socket_path()).await;
        let workspaces = list_workspaces(&mut connection).await;
        assert_eq!(workspaces.len(), 1, "{workspaces:?}");
        assert_eq!(workspaces[0].layout_rev, 3);
        assert!(workspaces[0].layout.terminal_sessions().is_empty());
        assert_eq!(workspaces[0].layout.root, editor_leaf("/src/main.rs"));
    });
}

/// Killing and persisting one workspace must not scrub or rewrite a sibling.
/// This covers the multi-workspace ledger as one snapshot rather than testing
/// each row in isolation.
#[test]
fn multiple_workspaces_stay_isolated_across_kill_and_restart() {
    let dir = TempDir::new().expect("temp dir");
    let first = server_named(&dir, "first.sock");
    let (left, right, right_session, right_layout) = smol::block_on(async {
        let mut connection = client(first.socket_path()).await;
        let (left, left_sessions) =
            create_workspace(&mut connection, dir.path(), Some("left")).await;
        let (right, right_sessions) =
            create_workspace(&mut connection, dir.path(), Some("right")).await;
        let left_layout = terminal_beside_editor(&left_sessions[0].id, "left.rs");
        let right_layout = terminal_beside_editor(&right_sessions[0].id, "right.rs");
        update_layout(&mut connection, &left.id, left_layout, 2).await;
        update_layout(&mut connection, &right.id, right_layout.clone(), 2).await;

        kill(&mut connection, &left_sessions[0].id).await;
        let (left_after, left_rows) = open_workspace(&mut connection, &left.id).await;
        assert!(left_rows.is_empty());
        assert_eq!(left_after.layout.root, editor_leaf("left.rs"));
        assert_eq!(left_after.layout_rev, 3);

        let (right_after, right_rows) = open_workspace(&mut connection, &right.id).await;
        assert_eq!(right_rows.len(), 1);
        assert_eq!(right_after.layout, right_layout);
        assert_eq!(right_after.layout_rev, 2);
        (left, right, right_sessions[0].id.clone(), right_layout)
    });
    drop(first);

    let second = server_named(&dir, "second.sock");
    smol::block_on(async {
        let mut connection = client(second.socket_path()).await;
        let (left_after, left_rows) = open_workspace(&mut connection, &left.id).await;
        assert!(left_rows.is_empty());
        assert_eq!(left_after.layout.root, editor_leaf("left.rs"));
        assert_eq!(left_after.layout_rev, 3);

        let (right_after, right_rows) = open_workspace(&mut connection, &right.id).await;
        assert_eq!(right_after.layout, right_layout);
        assert_eq!(right_after.layout_rev, 2);
        assert_eq!(right_rows.len(), 1);
        assert_eq!(right_rows[0].id, right_session);
        assert_eq!(right_rows[0].status, SessionStatus::Exited);
    });
}

/// The migration: a state file written before workspaces existed has sessions
/// with no workspace at all. Each one is wrapped in a workspace of its own,
/// automatically and without losing anything the file recorded.
#[test]
fn flat_sessions_from_an_older_daemon_are_migrated_into_workspaces() {
    let dir = TempDir::new().expect("temp dir");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).expect("creating the state dir");
    std::fs::write(
        state.join("sessions.json"),
        br#"{
          "version": 1,
          "sessions": [
            {
              "id": "flat-1",
              "agent_kind": "claude",
              "instance_label": "main",
              "cwd": "/home/u/proj",
              "created_at": 1754200000
            },
            {
              "id": "flat-2",
              "workspace_id": "ade-orphan",
              "agent_kind": "shell",
              "instance_label": "",
              "cwd": "/home/u/other/",
              "created_at": 1754200100
            }
          ]
        }"#,
    )
    .expect("writing the old state file");

    let server = server_named(&dir, "daemon.sock");
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let workspaces = list_workspaces(&mut connection).await;
        assert_eq!(workspaces.len(), 2, "{workspaces:?}");

        // Named from the session's label, rooted at its cwd, holding its tab.
        let first = &workspaces[0];
        assert_eq!(first.name, "main");
        assert_eq!(first.project_root, "/home/u/proj");
        assert_eq!(first.created_at, 1_754_200_000);
        assert_eq!(first.layout_rev, 1);
        assert_eq!(
            first.layout.terminal_sessions(),
            vec![SessionId::new("flat-1")]
        );

        // A session that named a workspace nobody recorded keeps that id, so
        // anything still referring to it keeps working; an empty label falls
        // back to the last component of the root.
        let second = &workspaces[1];
        assert_eq!(second.id, "ade-orphan");
        assert_eq!(second.name, "other");
        assert_eq!(second.project_root, "/home/u/other/");

        // The sessions themselves are still reported, as lost rows, each one
        // now claiming its new workspace.
        let sessions = list(&mut connection).await;
        assert_eq!(sessions.len(), 2, "{sessions:?}");
        assert_eq!(sessions[0].workspace_id, first.id);
        assert_eq!(sessions[1].workspace_id, "ade-orphan");
        assert!(sessions.iter().all(|s| s.status == SessionStatus::Exited));
    });
}

#[test]
fn handshake_succeeds_and_reports_the_daemon() {
    let (_dir, server) = server();
    smol::block_on(async {
        let stream = UnixStream::connect(server.socket_path())
            .await
            .expect("connecting");
        let mut connection = Connection::new(stream);
        let ack = connection
            .handshake(Hello {
                request_id: Some(1),
                ..Hello::current()
            })
            .await
            .expect("handshake");
        assert_eq!(ack.generation, MAX_GENERATION, "the daemon selects it");
        assert_eq!(ack.min_generation, MIN_GENERATION);
        assert_eq!(ack.max_generation, MAX_GENERATION);
        assert_eq!(
            ack.protocol_version, ack.generation,
            "the legacy field is informational and equal to the selection"
        );
        assert!(
            ack.capabilities.is_empty(),
            "generation 2 defines no capability, and an empty list says so: {:?}",
            ack.capabilities
        );
        assert!(!ack.degraded, "a fresh state dir is not a newer ledger");
        assert_eq!(ack.request_id, Some(1));
        assert_eq!(ack.host_os, std::env::consts::OS);
        assert!(!ack.daemon_version.is_empty());
    });
}

/// The one negotiation outcome that is fatal by design: no generation is common
/// to the two ranges, so there is no frame shape both ends could agree to speak
/// next and the daemon says so and closes.
#[test]
fn a_disjoint_generation_range_is_rejected() {
    let (_dir, server) = server();
    smol::block_on(async {
        let stream = UnixStream::connect(server.socket_path())
            .await
            .expect("connecting");
        let mut connection = Connection::new(stream);
        connection
            .send(&Frame::Hello(Hello {
                min_generation: MAX_GENERATION + 41,
                max_generation: MAX_GENERATION + 41,
                capabilities: Vec::new(),
                request_id: Some(3),
            }))
            .await
            .expect("sending hello");
        match connection.recv().await.expect("reply") {
            Frame::Error {
                code,
                message,
                request_id,
                ..
            } => {
                assert_eq!(code, error_code::UNSUPPORTED_GENERATION);
                assert_eq!(request_id, Some(3));
                // Both ranges: whoever reads this has to decide which end to
                // upgrade.
                assert!(
                    message.contains(&format!("{}..=", MAX_GENERATION + 41)),
                    "{message}"
                );
                assert!(
                    message.contains(&format!("{MIN_GENERATION}..={MAX_GENERATION}")),
                    "{message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(
            connection.recv().await.is_err(),
            "a failed negotiation closes the connection"
        );
    });
}

/// **The repeal.** An op this daemon has never heard of used to be a decode
/// error that broke the receive loop and took every attach on the connection
/// with it. It now costs exactly one request.
#[test]
fn an_unknown_op_gets_a_request_scoped_error_and_the_connection_survives() {
    let (_dir, server) = server();
    smol::block_on(async {
        let (mut connection, mut raw) = raw_client(server.socket_path()).await;

        send_raw(&mut raw, br#"{"op":"lease_renew","rid":9,"body":{}}"#).await;
        match recv(&mut connection, "the unknown-op refusal").await {
            Frame::Error {
                code,
                message,
                request_id,
                ..
            } => {
                assert_eq!(code, error_code::UNKNOWN_OP);
                assert_eq!(request_id, Some(9), "the rid is what correlates it");
                assert!(message.contains("lease_renew"), "{message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }

        // The same connection, still serving: this is the whole point.
        assert!(list(&mut connection).await.is_empty());
    });
}

/// The other request-scoped failure: the op is one this build implements and
/// its body is the wrong shape. Same treatment, different code — the sender's
/// bug is a different bug.
#[test]
fn a_malformed_body_gets_a_request_scoped_error() {
    let (_dir, server) = server();
    smol::block_on(async {
        let (mut connection, mut raw) = raw_client(server.socket_path()).await;

        send_raw(
            &mut raw,
            br#"{"op":"attach","rid":4,"body":{"session_id":42}}"#,
        )
        .await;
        match recv(&mut connection, "the malformed-body refusal").await {
            Frame::Error {
                code, request_id, ..
            } => {
                assert_eq!(code, error_code::MALFORMED_BODY);
                assert_eq!(request_id, Some(4));
            }
            other => panic!("expected Error, got {other:?}"),
        }

        assert!(list(&mut connection).await.is_empty());
    });
}

/// No `rid` means no reply could be correlated to anything, and the frame named
/// no session to report the failure against — so it is logged and dropped.
///
/// The bound is the `list` round trip: the outbound queue is FIFO, so an error
/// the daemon had queued would arrive *before* the list reply.
#[test]
fn a_rid_less_undecodable_frame_is_dropped() {
    let (_dir, server) = server();
    smol::block_on(async {
        let (mut connection, mut raw) = raw_client(server.socket_path()).await;

        send_raw(&mut raw, br#"{"op":"lease_renew","body":{}}"#).await;
        connection
            .send(&Frame::ListSessions {
                request_id: Some(11),
            })
            .await
            .expect("sending ListSessions");
        match recv(&mut connection, "the list reply").await {
            Frame::SessionList { request_id, .. } => assert_eq!(request_id, Some(11)),
            Frame::Error { code, .. } => {
                panic!("a rid-less frame was answered with {code}, not dropped")
            }
            other => panic!("expected SessionList, got {other:?}"),
        }
    });
}

/// A frame the daemon understands and will never accept from a client is
/// `invalid_argument` — and the refusal quotes it **bounded**. An `output` frame
/// may legally carry megabytes; Debug-formatting one into the error message made
/// the reply larger than `MAX_FRAME_BYTES`, so it could not be written, and the
/// failed write broke the writer task, the connection and every attach on it.
#[test]
fn an_unexpected_frame_is_refused_with_a_bounded_message() {
    let (_dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;

        connection
            .send(&Frame::Output {
                session_id: SessionId::new("s-1"),
                bytes: vec![b'x'; 1024 * 1024],
            })
            .await
            .expect("sending an output frame");
        match recv(&mut connection, "the refusal").await {
            Frame::Error { code, message, .. } => {
                assert_eq!(code, error_code::INVALID_ARGUMENT);
                assert!(
                    message.len() < 512,
                    "the refusal quoted the frame back: {} bytes",
                    message.len()
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }

        assert!(list(&mut connection).await.is_empty());
    });
}

/// A client-sent error is diagnostic, not a request: the attach client emits one
/// when a daemon frame fails to decode, and its output pump treats any error it
/// reads as terminal. Answering one would therefore end the very terminal the
/// rejection frame exists to keep alive, so the daemon logs it and says nothing.
///
/// The bound is the same FIFO `list` round trip as above.
#[test]
fn a_client_sent_error_is_ignored_and_the_connection_keeps_serving() {
    let (_dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;

        connection
            .send(&Frame::Error {
                session_id: None,
                workspace_id: None,
                code: error_code::UNKNOWN_OP.to_owned(),
                message: "this client cannot read that frame".to_owned(),
                request_id: None,
            })
            .await
            .expect("sending an error frame");
        connection
            .send(&Frame::ListSessions {
                request_id: Some(11),
            })
            .await
            .expect("sending ListSessions");
        match recv(&mut connection, "the list reply").await {
            Frame::SessionList { request_id, .. } => assert_eq!(request_id, Some(11)),
            Frame::Error { code, .. } => {
                panic!("an error about an error was answered with {code}")
            }
            other => panic!("expected SessionList, got {other:?}"),
        }
    });
}

/// The one decode failure the daemon may close over: stage one could not read
/// an envelope at all, so there is no `rid` and no reason to trust the next
/// four bytes either. `{"type":"Hello"}` is the pre-cut frame shape, which is
/// exactly what a client from before the cut would send.
#[test]
fn a_malformed_frame_is_rejected_and_the_connection_closes() {
    let (_dir, server) = server();
    smol::block_on(async {
        let (mut connection, mut raw) = raw_client(server.socket_path()).await;

        send_raw(&mut raw, br#"{"type":"Hello"}"#).await;
        match recv(&mut connection, "the malformed-frame refusal").await {
            Frame::Error {
                code, request_id, ..
            } => {
                assert_eq!(code, error_code::MALFORMED_FRAME);
                assert_eq!(request_id, None, "there was no rid to read");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(
            matches!(connection.recv().await, Err(ReadFrameError::Transport(_))),
            "a peer that cannot frame an envelope is closed on"
        );
    });
}

/// §8.5 through the handshake: a daemon whose ledger was written by a newer
/// schema serves normally but never writes, and has to say so — a client that
/// does not know cannot warn that work here will not survive a restart.
#[test]
fn hello_reports_degraded_when_the_ledger_is_newer() {
    let dir = TempDir::new().expect("temp dir");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).expect("the state dir");
    std::fs::write(
        state.join("sessions.json"),
        br#"{"version": 99, "sessions": [], "workspaces": [], "unknown_future_table": []}"#,
    )
    .expect("seeding a newer ledger");

    let config = ServerConfig::new(dir.path().join("daemon.sock"), &state);
    let server = Server::spawn(config).expect("spawning server");
    smol::block_on(async {
        let stream = UnixStream::connect(server.socket_path())
            .await
            .expect("connecting");
        let ack = Connection::new(stream)
            .handshake(Hello::current())
            .await
            .expect("handshake");
        assert!(
            ack.degraded,
            "the ledger is from the future and this daemon will not rewrite it"
        );
    });
}

/// A duplicate identifier is an encoding artefact with one unambiguous reading
/// — the peer is capable — so it is deduplicated and the handshake stands.
#[test]
fn a_duplicated_capability_still_handshakes() {
    let (_dir, server) = server();
    smol::block_on(async {
        let stream = UnixStream::connect(server.socket_path())
            .await
            .expect("connecting");
        let ack = Connection::new(stream)
            .handshake(Hello {
                capabilities: vec!["history".to_owned(), "history".to_owned()],
                ..Hello::current()
            })
            .await
            .expect("handshake");
        assert_eq!(ack.generation, MAX_GENERATION);
        assert!(
            ack.capabilities.is_empty(),
            "an identifier this daemon has never heard of simply falls out"
        );
    });
}

/// The bounds are the one capability rule that is fatal, and the refusal has to
/// name the offender: whoever reads it is looking at a log line, not the list.
#[test]
fn a_capability_list_past_its_bounds_is_rejected() {
    let (_dir, server) = server();
    smol::block_on(async {
        let stream = UnixStream::connect(server.socket_path())
            .await
            .expect("connecting");
        let mut connection = Connection::new(stream);
        connection
            .send(&Frame::Hello(Hello {
                capabilities: (0..257).map(|i| format!("cap{i}")).collect(),
                request_id: Some(8),
                ..Hello::current()
            }))
            .await
            .expect("sending hello");
        match connection.recv().await.expect("reply") {
            Frame::Error {
                code,
                message,
                request_id,
                ..
            } => {
                assert_eq!(code, error_code::INVALID_ARGUMENT);
                assert_eq!(request_id, Some(8));
                assert!(message.contains("257"), "{message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(
            connection.recv().await.is_err(),
            "an unusable capability list is fatal to the handshake"
        );
    });
}

/// Naming the offender must not mean carrying it. A capability whose *length*
/// is the violation is quoted back bounded, because a refusal that repeated
/// megabytes would be a frame `write_frame` refuses to write — and the client
/// would then see EOF, which reads as a pre-cut daemon (§6.1) rather than as
/// the bad capability it is.
#[test]
fn a_huge_capability_is_refused_with_a_frame_small_enough_to_send() {
    let (_dir, server) = server();
    smol::block_on(async {
        let stream = UnixStream::connect(server.socket_path())
            .await
            .expect("connecting");
        let mut connection = Connection::new(stream);
        connection
            .send(&Frame::Hello(Hello {
                capabilities: vec!["a".repeat(4 * 1024 * 1024)],
                request_id: Some(9),
                ..Hello::current()
            }))
            .await
            .expect("sending hello");
        match connection.recv().await.expect("reply") {
            Frame::Error {
                code,
                message,
                request_id,
                ..
            } => {
                assert_eq!(code, error_code::INVALID_ARGUMENT);
                assert_eq!(request_id, Some(9));
                assert!(message.contains("4194304 bytes"), "{message}");
                assert!(
                    message.len() < 1024,
                    "the refusal quoted the capability back: {} bytes",
                    message.len()
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    });
}

#[test]
fn created_session_is_listed() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let session = create(&mut connection, dir.path(), "sleep 300").await;
        assert_eq!(session.workspace_id, "ws-1");
        // A just-launched agent is booting, not idle.
        assert_eq!(session.status, SessionStatus::Working);

        let sessions = list(&mut connection).await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session.id);
        assert_eq!(sessions[0].instance_label, "test");
    });
}

#[test]
fn kill_removes_the_session_and_unknown_ids_error() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let session = create(&mut connection, dir.path(), "sleep 300").await;

        connection
            .send(&Frame::Kill {
                session_id: session.id.clone(),
                request_id: Some(5),
            })
            .await
            .expect("sending Kill");
        match connection.recv().await.expect("reply") {
            Frame::Removed { session_id } => assert_eq!(session_id, session.id),
            other => panic!("expected Removed, got {other:?}"),
        }
        assert!(list(&mut connection).await.is_empty());

        let ghost = SessionId::new("no-such-session");
        connection
            .send(&Frame::Kill {
                session_id: ghost.clone(),
                request_id: Some(6),
            })
            .await
            .expect("sending Kill");
        match connection.recv().await.expect("reply") {
            Frame::Error {
                session_id,
                code,
                request_id,
                ..
            } => {
                assert_eq!(session_id, Some(ghost));
                assert_eq!(code, error_code::NOT_FOUND);
                assert_eq!(request_id, Some(6));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    });
}

/// Kill means dead, not "asked politely".
///
/// A `Kill` that only sends SIGHUP leaves a child that ignores it running with
/// no row to reach it by — the orphan behind the reported collision, where a
/// second agent could not resume a thread a forgotten first one still held. The
/// command here ignores SIGHUP and never reads its pty, so neither the signal
/// nor the pty hangup that follows the master's close can end it: only the
/// escalation can. It records its own pid, which is also its process-group id,
/// because the wire carries no pid.
#[test]
fn kill_reaches_a_child_that_ignores_sighup() {
    let (dir, server) = server();
    let pid_file = dir.path().join("survivor.pid");
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let command = format!(
            "sh -c 'trap \"\" HUP; echo $$ > {}; while :; do sleep 1; done'",
            pid_file.display()
        );
        let session = create(&mut connection, dir.path(), &command).await;
        let pid = wait_for_pid(&pid_file).await;

        kill(&mut connection, &session.id).await;

        assert_dies(pid, true, "the killed session's SIGHUP-ignoring child").await;
    });
}

/// A workspace close is a kill, with the same escalation: nothing in the
/// removed workspace may outlive it just because it arrived via
/// `KillWorkspace` instead of `Kill`.
#[test]
fn kill_workspace_reaches_a_child_that_ignores_sighup() {
    let (dir, server) = server();
    let pid_file = dir.path().join("survivor.pid");
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let command = format!(
            "sh -c 'trap \"\" HUP; echo $$ > {}; while :; do sleep 1; done'",
            pid_file.display()
        );
        let session = create(&mut connection, dir.path(), &command).await;
        let pid = wait_for_pid(&pid_file).await;

        connection
            .send(&Frame::KillWorkspace {
                workspace_id: session.workspace_id.clone(),
                request_id: Some(66),
            })
            .await
            .expect("sending KillWorkspace");

        assert_dies(pid, true, "the closed workspace's SIGHUP-ignoring child").await;
    });
}

/// The escalation must fire even when the *leader* dies of the SIGHUP: a
/// background descendant that traps it outlives the leader, and only the
/// group SIGKILL can reach it.
#[test]
fn kill_reaches_a_descendant_that_outlives_its_leader() {
    let (dir, server) = server();
    let pid_file = dir.path().join("descendant.pid");
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let command = format!(
            "sh -c '(trap \"\" HUP; while :; do sleep 1; done) & echo $! > {}; wait'",
            pid_file.display()
        );
        let session = create(&mut connection, dir.path(), &command).await;
        let pid = wait_for_pid(&pid_file).await;

        kill(&mut connection, &session.id).await;

        assert_dies(
            pid,
            false,
            "the killed session's leader-outliving descendant",
        )
        .await;
    });
}

/// Poll until `pid` is gone — or SIGKILL what the test leaked and fail.
///
/// Generous against the daemon's own grace period: the assertion is that the
/// process eventually goes, not how fast.
async fn assert_dies(pid: libc::pid_t, whole_group: bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while running(pid) && Instant::now() < deadline {
        pause(Duration::from_millis(100)).await;
    }
    if running(pid) {
        // SAFETY: a plain `kill(2)` on a process (group) this test caused to
        // exist, so the failing test leaks nothing.
        unsafe { libc::kill(if whole_group { -pid } else { pid }, libc::SIGKILL) };
        panic!("{what}: {pid} survived");
    }
}

/// The pid the command under test wrote, once it has written it.
async fn wait_for_pid(path: &Path) -> libc::pid_t {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse::<libc::pid_t>()
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "the command never wrote its pid");
        pause(Duration::from_millis(50)).await;
    }
}

/// Does this pid still exist? A zombie counts as gone: the daemon's reaper
/// waits on the child, so an unreaped pid means the process is genuinely there.
fn running(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 checks for the process without sending anything.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// The point of the daemon: an agent that dies stays visible instead of
/// vanishing from the sidebar.
#[test]
fn a_session_whose_command_exits_stays_listed_as_exited() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let session = create(&mut connection, dir.path(), "true").await;
        let sessions = wait_for(&mut connection, "the session to exit", |sessions| {
            sessions.iter().any(|s| s.status == SessionStatus::Exited)
        })
        .await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session.id);
    });
}

/// Disconnecting is not killing. Nothing but `Kill` ends a session.
#[test]
fn client_disconnect_keeps_sessions_alive() {
    let (dir, server) = server();
    smol::block_on(async {
        let session = {
            let mut connection = client(server.socket_path()).await;
            create(&mut connection, dir.path(), "sleep 300").await
        };

        let mut second = client(server.socket_path()).await;
        let sessions = list(&mut second).await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session.id);
        // Which live status it derives is timing; that it is not `Exited` is
        // the invariant under test.
        assert_ne!(sessions[0].status, SessionStatus::Exited);
    });
}

/// Crash honesty: a restarted daemon reports what it lost rather than
/// pretending it never existed.
#[test]
fn sessions_from_a_previous_daemon_are_reported_as_lost() {
    let dir = TempDir::new().expect("temp dir");
    let first = server_named(&dir, "first.sock");
    let session = smol::block_on(async {
        let mut connection = client(first.socket_path()).await;
        create(&mut connection, dir.path(), "sleep 300").await
    });
    assert!(dir.path().join("state").join("sessions.json").exists());

    let second = server_named(&dir, "second.sock");
    smol::block_on(async {
        let mut connection = client(second.socket_path()).await;
        let sessions = list(&mut connection).await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session.id);
        assert_eq!(sessions[0].status, SessionStatus::Exited);
        assert!(
            sessions[0].instance_label.ends_with("(lost)"),
            "{}",
            sessions[0].instance_label
        );
    });

    // Reported once, then forgotten: a third daemon must not replay the same
    // tombstone forever.
    let third = server_named(&dir, "third.sock");
    smol::block_on(async {
        let mut connection = client(third.socket_path()).await;
        assert!(list(&mut connection).await.is_empty());
    });
}

#[test]
fn two_connections_are_served_concurrently() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut first = client(server.socket_path()).await;
        let mut second = client(server.socket_path()).await;

        let a = create(&mut first, dir.path(), "sleep 300").await;
        let b = create(&mut second, dir.path(), "sleep 300").await;
        assert_ne!(a.id, b.id);

        let seen_by_first = list(&mut first).await;
        let seen_by_second = list(&mut second).await;
        assert_eq!(seen_by_first.len(), 2);
        assert_eq!(seen_by_first, seen_by_second);
    });
}

/// A second daemon on a live socket must refuse to start rather than strand
/// the first one's sessions.
#[test]
fn a_second_daemon_on_a_live_socket_refuses_to_start() {
    let (dir, server) = server();
    let config = ServerConfig::new(server.socket_path(), dir.path().join("state"));
    let error = match Server::bind(config) {
        Ok(_) => panic!("second bind must fail"),
        Err(error) => error,
    };
    assert!(
        format!("{error:#}").contains("already running"),
        "{error:#}"
    );
}

/// Scrollback is why the daemon exists: output produced while nobody was
/// watching is still there when someone attaches.
#[test]
fn attach_replays_output_produced_before_attaching() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let session = create(&mut connection, dir.path(), "printf hello").await;
        wait_for(&mut connection, "the session to exit", |sessions| {
            sessions.iter().any(|s| s.status == SessionStatus::Exited)
        })
        .await;

        let mut viewer = client(server.socket_path()).await;
        let (replayed, truncated) = replay_containing(&mut viewer, &session.id, b"hello").await;
        assert!(!truncated, "2 MiB of scrollback cannot have wrapped");
        assert!(contains(&replayed, b"hello"), "{replayed:?}");
    });
}

/// A late attach gets the terminal ending after its replay. This is what lets
/// a client reconnect after an outage in which the child exited.
#[test]
fn attaching_after_exit_replays_then_reports_the_exit() {
    let (dir, server) = fast_server();
    smol::block_on(async {
        let mut owner = client(server.socket_path()).await;
        let session = create(&mut owner, dir.path(), "sh -c 'printf goodbye; exit 7'").await;
        let mut first = client(server.socket_path()).await;
        let (mut seen, _) = attach(&mut first, &session.id).await;
        let first_exit = loop {
            match recv(&mut first, "the first attach to end").await {
                Frame::Output { bytes, .. } => seen.extend_from_slice(&bytes),
                Frame::Exited { exit_code, .. } => break exit_code,
                _ => {}
            }
        };
        assert!(contains(&seen, b"goodbye"), "{seen:?}");
        assert_eq!(first_exit, Some(7));

        let mut viewer = client(server.socket_path()).await;
        let (replayed, _) = attach(&mut viewer, &session.id).await;
        assert!(contains(&replayed, b"goodbye"), "{replayed:?}");
        match recv(&mut viewer, "Exited after Replay").await {
            Frame::Exited {
                session_id,
                exit_code,
            } => {
                assert_eq!(session_id, session.id);
                assert_eq!(exit_code, Some(7));
            }
            other => panic!("expected Exited, got {other:?}"),
        }
    });
}

/// The live half: bytes the pty produces after attaching arrive as `Output`.
#[test]
fn attach_streams_live_output() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let session = cat_session(&server, &mut connection, dir.path()).await;
        attach(&mut connection, &session.id).await;

        write_to(&mut connection, &session.id, b"ping\n").await;
        let live = output_until(&mut connection, &session.id, b"ping\r\n").await;
        assert!(contains(&live, b"ping\r\n"), "{live:?}");
    });
}

/// The ordering guarantee: the replay covers everything up to the attach, the
/// live stream covers everything after it, and nothing is dropped or repeated
/// at the seam.
///
/// The replay being a repaint rather than raw bytes does not weaken this â€” it
/// is why the repaint is synthesized under the same lock that fans out live
/// output. `alpha` was on the screen when the attach happened and appears once,
/// in the repaint; `beta` arrived after and appears once, live.
#[test]
fn replay_and_live_output_neither_gap_nor_duplicate() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let session = cat_session(&server, &mut connection, dir.path()).await;

        write_to(&mut connection, &session.id, b"alpha\n").await;
        // On the *screen*, so the needle is the text and not the newline that
        // put it there: a repaint paints rows with `CUP`.
        wait_for_ring(server.socket_path(), &session.id, b"alpha").await;

        let (replayed, _) = attach(&mut connection, &session.id).await;
        write_to(&mut connection, &session.id, b"beta\n").await;
        let live = output_until(&mut connection, &session.id, b"beta\r\n").await;

        let mut seen = replayed;
        seen.extend_from_slice(&live);
        assert_eq!(occurrences(&seen, b"alpha"), 1, "{seen:?}");
        assert_eq!(occurrences(&seen, b"beta"), 1, "{seen:?}");
        let alpha = seen
            .windows(5)
            .position(|window| window == b"alpha")
            .expect("alpha");
        let beta = seen
            .windows(4)
            .position(|window| window == b"beta")
            .expect("beta");
        assert!(alpha < beta, "replay must precede live output");
    });
}

/// Fan-out: one session, many watchers. Nobody's attach steals another's
/// output.
#[test]
fn two_connections_attached_to_one_session_both_receive_output() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut first = client(server.socket_path()).await;
        let mut second = client(server.socket_path()).await;
        let session = cat_session(&server, &mut first, dir.path()).await;

        attach(&mut first, &session.id).await;
        attach(&mut second, &session.id).await;
        write_to(&mut first, &session.id, b"ping\n").await;

        let by_first = output_until(&mut first, &session.id, b"ping\r\n").await;
        let by_second = output_until(&mut second, &session.id, b"ping\r\n").await;
        assert!(contains(&by_first, b"ping\r\n"), "{by_first:?}");
        assert!(contains(&by_second, b"ping\r\n"), "{by_second:?}");
    });
}

/// Detach is not kill. It stops one connection's stream and touches nothing
/// else.
#[test]
fn detach_stops_output_without_touching_the_session() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut first = client(server.socket_path()).await;
        let mut second = client(server.socket_path()).await;
        let session = cat_session(&server, &mut first, dir.path()).await;

        attach(&mut first, &session.id).await;
        attach(&mut second, &session.id).await;
        write_to(&mut first, &session.id, b"first\n").await;
        output_until(&mut first, &session.id, b"first\r\n").await;
        output_until(&mut second, &session.id, b"first\r\n").await;

        detach(&mut first, &session.id).await;
        // Frames from one connection are handled in order, so the detach is
        // fully applied before this write is.
        write_to(&mut first, &session.id, b"second\n").await;
        let by_second = output_until(&mut second, &session.id, b"second\r\n").await;
        assert!(contains(&by_second, b"second\r\n"), "{by_second:?}");

        // Everything this connection is owed is queued in order, so the very
        // next frame being the list reply proves no `Output` leaked to it.
        first
            .send(&Frame::ListSessions {
                request_id: Some(11),
            })
            .await
            .expect("sending ListSessions");
        match recv(&mut first, "SessionList").await {
            Frame::SessionList { sessions, .. } => {
                assert_eq!(sessions.len(), 1);
                assert_eq!(sessions[0].id, session.id);
                assert_ne!(sessions[0].status, SessionStatus::Exited);
            }
            other => panic!("expected SessionList, got {other:?}"),
        }
    });
}

/// A dropped connection detaches implicitly â€” and only implicitly. The pty
/// keeps running and its scrollback survives for whoever reconnects.
#[test]
fn dropping_a_connection_detaches_but_never_kills() {
    let (dir, server) = server();
    smol::block_on(async {
        let session = {
            let mut connection = client(server.socket_path()).await;
            let session = cat_session(&server, &mut connection, dir.path()).await;
            attach(&mut connection, &session.id).await;
            write_to(&mut connection, &session.id, b"before\n").await;
            output_until(&mut connection, &session.id, b"before\r\n").await;
            session
        };

        // Nobody is attached now. Input from a short-lived replacement still
        // reaches the pty, and the resulting output must enter scrollback.
        {
            let mut unattached = client(server.socket_path()).await;
            write_to(&mut unattached, &session.id, b"while-away\n").await;
            wait_for_ring(server.socket_path(), &session.id, b"while-away").await;
        }

        let mut reconnected = client(server.socket_path()).await;
        let (replayed, _) = attach(&mut reconnected, &session.id).await;
        // The repaint paints rows with `CUP`, so what survives is the text on
        // the screen rather than the line ending that put it there.
        assert!(contains(&replayed, b"before"), "{replayed:?}");
        assert_eq!(occurrences(&replayed, b"before"), 1, "{replayed:?}");
        assert_eq!(occurrences(&replayed, b"while-away"), 1, "{replayed:?}");

        let mut observer = client(server.socket_path()).await;
        let sessions = list(&mut observer).await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session.id);
        assert_ne!(sessions[0].status, SessionStatus::Exited);

        // Still writable, i.e. still a live pty and not a corpse.
        write_to(&mut reconnected, &session.id, b"after\n").await;
        let live = output_until(&mut reconnected, &session.id, b"after\r\n").await;
        assert!(contains(&live, b"after\r\n"), "{live:?}");
    });
}

/// An attach repaints at the size the session was last *resized* to, not the
/// size it was created at.
///
/// This is the whole reason the daemon keeps a screen at all: a client
/// re-mounting a terminal view into a smaller split would otherwise be handed
/// scrollback that is only correct at the width it was produced at.
#[test]
fn attach_repaints_at_the_last_resized_size() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let session = cat_session(&server, &mut connection, dir.path()).await;

        // Twelve lines on a 24-row screen, then a screen half that tall.
        for line in 1..=12 {
            write_to(
                &mut connection,
                &session.id,
                format!("line{line}\n").as_bytes(),
            )
            .await;
        }
        wait_for_ring(server.socket_path(), &session.id, b"line12").await;
        connection
            .send(&Frame::Resize {
                session_id: session.id.clone(),
                cols: 80,
                rows: 8,
            })
            .await
            .expect("sending Resize");

        let mut viewer = client(server.socket_path()).await;
        let (replayed, truncated) = attach(&mut viewer, &session.id).await;
        // Nothing may be painted onto a row the screen no longer has.
        assert!(
            !contains(&replayed, b"\x1b[9;1H"),
            "painted a ninth row onto an eight-row screen: {replayed:?}"
        );
        // The newest output is what survives a shrink, and the screen says so.
        assert!(contains(&replayed, b"line12"), "{replayed:?}");
        assert!(truncated, "content scrolled off the top to make room");
    });
}

/// A full-screen app is replayed as the alternate screen, and leaving it puts
/// the primary screen back.
///
/// `cat` is the vehicle: it echoes whatever is written to it, so the escape
/// sequences arrive at the session's screen exactly as an app would have
/// produced them. The bug this closes is the one a user sees as leftover htop
/// rows above their prompt â€” with a single-buffer emulator the app's last frame
/// stayed on the primary screen after it exited.
#[test]
fn the_alternate_screen_is_replayed_and_then_given_back() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let session = cat_session(&server, &mut connection, dir.path()).await;

        write_to(&mut connection, &session.id, b"primary-output\n").await;
        wait_for_ring(server.socket_path(), &session.id, b"primary-output").await;

        write_to(
            &mut connection,
            &session.id,
            b"\x1b[?1049h\x1b[HALTSCREEN\n",
        )
        .await;
        wait_for_ring(server.socket_path(), &session.id, b"ALTSCREEN").await;

        let mut viewer = client(server.socket_path()).await;
        let (in_app, _) = attach(&mut viewer, &session.id).await;
        assert!(
            contains(&in_app, b"\x1b[?1049h"),
            "the client is put on the alternate screen too: {in_app:?}"
        );
        assert!(contains(&in_app, b"ALTSCREEN"), "{in_app:?}");
        detach(&mut viewer, &session.id).await;

        // The app exits.
        write_to(&mut connection, &session.id, b"\x1b[?1049l\n").await;
        wait_for_ring(server.socket_path(), &session.id, b"primary-output").await;

        let (after, _) = attach(&mut viewer, &session.id).await;
        assert!(
            !contains(&after, b"\x1b[?1049h"),
            "back on the primary screen: {after:?}"
        );
        assert!(
            contains(&after, b"primary-output"),
            "the primary screen survived the app: {after:?}"
        );
        assert!(
            !contains(&after, b"ALTSCREEN"),
            "and none of the app is left on it: {after:?}"
        );
    });
}

/// A session that printed more than fits keeps the newest of it and says so.
///
/// `truncated` is now either half of "this is not everything the session
/// printed": the ring dropped bytes, or the screen scrolled. Here both are
/// true, and what comes back is the last screenful â€” the head is what a
/// terminal can afford to lose.
#[test]
fn scrollback_wraps_to_the_tail_and_reports_truncation() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let session =
            create_with_scrollback(&mut connection, dir.path(), "seq 1 500", Some(64)).await;
        wait_for(&mut connection, "the session to exit", |sessions| {
            sessions.iter().any(|s| s.status == SessionStatus::Exited)
        })
        .await;

        let mut viewer = client(server.socket_path()).await;
        let (replayed, truncated) = replay_containing(&mut viewer, &session.id, b"500").await;
        assert!(
            truncated,
            "500 lines cannot fit in 64 bytes or on one screen"
        );
        // The newest lines are on the screen; the ones that scrolled past the
        // top of it are gone, and 250 went long before the end.
        assert!(!contains(&replayed, b"250"), "{replayed:?}");
    });
}

/// Resize is accepted silently and changes nothing else; writing to a session
/// that is gone is an error rather than a silent no-op.
#[test]
fn resize_is_silent_and_writing_to_a_killed_session_errors() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let session = cat_session(&server, &mut connection, dir.path()).await;

        connection
            .send(&Frame::Resize {
                session_id: session.id.clone(),
                cols: 120,
                rows: 40,
            })
            .await
            .expect("sending Resize");

        // No reply is owed for a successful resize, so the next frame is the
        // list reply â€” and the session is still there to be listed.
        connection
            .send(&Frame::ListSessions {
                request_id: Some(11),
            })
            .await
            .expect("sending ListSessions");
        match recv(&mut connection, "SessionList").await {
            Frame::SessionList { sessions, .. } => {
                assert_eq!(sessions.len(), 1);
                assert_ne!(sessions[0].status, SessionStatus::Exited);
            }
            other => panic!("expected SessionList, got {other:?}"),
        }

        connection
            .send(&Frame::Kill {
                session_id: session.id.clone(),
                request_id: Some(5),
            })
            .await
            .expect("sending Kill");
        match recv(&mut connection, "Removed").await {
            Frame::Removed { session_id } => assert_eq!(session_id, session.id),
            other => panic!("expected Removed, got {other:?}"),
        }

        write_to(&mut connection, &session.id, b"ping\n").await;
        match recv(&mut connection, "Error").await {
            Frame::Error {
                session_id,
                code,
                message,
                request_id,
                ..
            } => {
                assert_eq!(session_id, Some(session.id));
                assert_eq!(code, error_code::NOT_FOUND);
                // The legal unsolicited error: no rid to echo, but a session
                // named, so a client routes it to diagnostics.
                assert_eq!(request_id, None);
                assert!(message.contains("no such session"), "{message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    });
}

/// A socket file left behind by a crashed daemon is not a reason to refuse.
#[test]
fn a_stale_socket_file_is_removed() {
    let dir = TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    std::fs::write(&socket, b"stale").expect("writing stale socket file");
    let config = ServerConfig::new(&socket, dir.path().join("state"));
    let server = Server::spawn(config).expect("spawning over a stale socket");
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        assert!(list(&mut connection).await.is_empty());
    });
}

// ---------------------------------------------------------------------------
// Status derivation and the push event stream
// ---------------------------------------------------------------------------

/// Subscribe and wait until the daemon has acted on it, returning the initial
/// snapshot as `(id, status, since)`.
///
/// The `ListSessions` round trip is the barrier that makes these tests
/// deterministic: frames from one connection are handled in order, so the list
/// reply cannot come back before the subscription is live â€” and without it a
/// `CreateSession` on *another* connection could be handled first and its
/// `Created` event lost.
async fn subscribe_and_snapshot(
    connection: &mut Connection<UnixStream>,
) -> Vec<(SessionId, SessionStatus, u64)> {
    subscribe(connection).await;
    connection
        .send(&Frame::ListSessions {
            request_id: Some(11),
        })
        .await
        .expect("sending ListSessions");
    let mut snapshot = Vec::new();
    loop {
        match recv(connection, "the subscribe snapshot").await {
            Frame::Status {
                session_id,
                status,
                since,
            } => snapshot.push((session_id, status, since)),
            Frame::SessionList { .. } => return snapshot,
            other => panic!("expected Status or SessionList, got {other:?}"),
        }
    }
}

async fn kill(connection: &mut Connection<UnixStream>, id: &SessionId) {
    connection
        .send(&Frame::Kill {
            session_id: id.clone(),
            request_id: Some(41),
        })
        .await
        .expect("sending Kill");
    event_until(connection, "Removed", |frame| match frame {
        Frame::Removed { session_id } if session_id == id => Some(()),
        _ => None,
    })
    .await;
}

/// Park this thread; the server runs on the global executor's own threads.
async fn pause(duration: Duration) {
    smol::unblock(move || std::thread::sleep(duration)).await;
}

fn status_of(sessions: &[SessionInfo], id: &SessionId) -> SessionStatus {
    sessions
        .iter()
        .find(|session| &session.id == id)
        .unwrap_or_else(|| panic!("session {id} is not listed"))
        .status
}

/// Subscribing is answered by a status for everything the daemon knows about,
/// including the sessions whose process is already gone.
#[test]
fn subscribe_pushes_a_status_snapshot_for_every_session() {
    let (dir, server) = fast_server();
    smol::block_on(async {
        let mut owner = client(server.socket_path()).await;
        let alive = create(&mut owner, dir.path(), "sleep 300").await;
        let gone = create(&mut owner, dir.path(), "true").await;
        wait_for(&mut owner, "the short session to exit", |sessions| {
            sessions
                .iter()
                .any(|s| s.id == gone.id && s.status == SessionStatus::Exited)
        })
        .await;

        let mut watcher = client(server.socket_path()).await;
        let snapshot = subscribe_and_snapshot(&mut watcher).await;
        assert_eq!(snapshot.len(), 2, "{snapshot:?}");
        let statuses: Vec<(SessionId, SessionStatus)> = snapshot
            .iter()
            .map(|(id, status, _)| (id.clone(), *status))
            .collect();
        assert!(
            statuses.contains(&(gone.id.clone(), SessionStatus::Exited)),
            "{statuses:?}"
        );
        let live = statuses
            .iter()
            .find(|(id, _)| id == &alive.id)
            .expect("the live session");
        assert_ne!(live.1, SessionStatus::Exited);
        assert!(
            snapshot.iter().all(|(_, _, since)| *since > 0),
            "{snapshot:?}"
        );

        // Subscribing twice just resends the snapshot.
        let again = subscribe_and_snapshot(&mut watcher).await;
        assert_eq!(again.len(), 2, "{again:?}");

        kill(&mut owner, &alive.id).await;
    });
}

/// A subscriber sees sessions appear and disappear without asking.
#[test]
fn created_and_removed_events_reach_a_subscriber() {
    let (dir, server) = fast_server();
    smol::block_on(async {
        let mut watcher = client(server.socket_path()).await;
        assert!(subscribe_and_snapshot(&mut watcher).await.is_empty());

        let mut owner = client(server.socket_path()).await;
        let session = create(&mut owner, dir.path(), "sleep 300").await;
        let created = event_until(&mut watcher, "Created", |frame| match frame {
            Frame::Created {
                session,
                request_id,
            } => Some((session.clone(), *request_id)),
            _ => None,
        })
        .await;
        assert_eq!(created.0.id, session.id);
        assert_eq!(created.1, None, "an event is nobody's reply");

        kill(&mut owner, &session.id).await;
        event_until(&mut watcher, "Removed", |frame| match frame {
            Frame::Removed { session_id } if session_id == &session.id => Some(()),
            _ => None,
        })
        .await;
    });
}

/// A session event must never name a workspace the subscriber has not learned
/// about yet. This is the direct `CreateSession` path, where the daemon mints
/// the missing workspace automatically.
#[test]
fn an_implicit_workspace_is_announced_before_its_first_session() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut watcher = client(server.socket_path()).await;
        assert!(subscribe_and_snapshot(&mut watcher).await.is_empty());

        let mut owner = client(server.socket_path()).await;
        let created = create(&mut owner, dir.path(), "sleep 300").await;
        match event_until(
            &mut watcher,
            "Workspace before Created",
            |frame| match frame {
                Frame::Workspace {
                    workspace,
                    sessions,
                    ..
                } => Some(Ok((workspace.clone(), sessions.clone()))),
                Frame::Created { session, .. } if session.id == created.id => {
                    Some(Err(session.clone()))
                }
                _ => None,
            },
        )
        .await
        {
            Ok((workspace, sessions)) => {
                assert_eq!(workspace.id, created.workspace_id);
                assert_eq!(sessions.len(), 1);
                assert_eq!(sessions[0].id, created.id);
            }
            Err(session) => panic!(
                "session {} was announced before workspace {}",
                session.id, session.workspace_id
            ),
        }
    });
}

/// `CreateWorkspace` is the same transaction through a different request: its
/// observer event also precedes the session event it makes meaningful.
#[test]
fn a_created_workspace_is_announced_before_its_first_session() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut watcher = client(server.socket_path()).await;
        assert!(subscribe_and_snapshot(&mut watcher).await.is_empty());

        let mut owner = client(server.socket_path()).await;
        let (created, sessions) = create_workspace(&mut owner, dir.path(), Some("ordered")).await;
        let session = sessions[0].clone();
        match event_until(
            &mut watcher,
            "Workspace before Created",
            |frame| match frame {
                Frame::Workspace {
                    workspace,
                    sessions,
                    ..
                } if workspace.id == created.id => Some(Ok(sessions.clone())),
                Frame::Created { session: seen, .. } if seen.id == session.id => {
                    Some(Err(seen.clone()))
                }
                _ => None,
            },
        )
        .await
        {
            Ok(announced) => assert_eq!(announced, vec![session]),
            Err(seen) => panic!(
                "session {} was announced before workspace {}",
                seen.id, seen.workspace_id
            ),
        }
    });
}

/// The death of an agent is pushed once, with whatever the wait reported.
#[test]
fn an_exited_child_pushes_exited_and_then_the_exited_status() {
    let (dir, server) = fast_server();
    smol::block_on(async {
        let mut watcher = client(server.socket_path()).await;
        assert!(subscribe_and_snapshot(&mut watcher).await.is_empty());

        let mut owner = client(server.socket_path()).await;
        let session = create(&mut owner, dir.path(), "sh -c 'exit 7'").await;

        let exit_code = event_until(&mut watcher, "Exited", |frame| match frame {
            Frame::Exited {
                session_id,
                exit_code,
            } if session_id == &session.id => Some(*exit_code),
            _ => None,
        })
        .await;
        // `None` would mean the platform's pty layer masked the code; the
        // event arriving at all is the part that is not negotiable.
        if let Some(code) = exit_code {
            assert_eq!(code, 7);
        }

        status_becomes(&mut watcher, &session.id, SessionStatus::Exited).await;
        // The row outlives the process: it is still listed.
        assert_eq!(
            status_of(&list(&mut owner).await, &session.id),
            SessionStatus::Exited
        );
    });
}

/// Recent output is `Working` â€” including when the foreground process is a
/// shell, which is why the recency rule is checked before the idle rule. Once
/// the output stops and the child is still alive, it is `NeedsInput`.
#[test]
fn output_means_working_and_silence_means_needs_input() {
    let (dir, server) = fast_server();
    smol::block_on(async {
        let mut watcher = client(server.socket_path()).await;
        assert!(subscribe_and_snapshot(&mut watcher).await.is_empty());
        let mut owner = client(server.socket_path()).await;

        // Prints every ~20ms, well inside the 300ms threshold, from a process
        // whose name is `sh`.
        let chatty = create(
            &mut owner,
            dir.path(),
            "sh -c 'while :; do printf .; sleep 0.02; done'",
        )
        .await;
        assert_eq!(chatty.status, SessionStatus::Working);
        pause(Duration::from_millis(900)).await;
        assert_eq!(
            status_of(&list(&mut owner).await, &chatty.id),
            SessionStatus::Working,
            "output inside the threshold outranks the idle rule"
        );

        // One burst, then a live child that says nothing at all.
        let quiet = create(&mut owner, dir.path(), "sh -c 'printf go; exec sleep 30'").await;
        let since = status_becomes(&mut watcher, &quiet.id, SessionStatus::NeedsInput).await;
        assert!(since > 0);
        assert_eq!(
            status_of(&list(&mut owner).await, &quiet.id),
            SessionStatus::NeedsInput,
            "the derived status is what ListSessions reports"
        );

        kill(&mut owner, &chatty.id).await;
        kill(&mut owner, &quiet.id).await;
    });
}

/// A bell means "look at me" even while the output is fresh, and stops meaning
/// it as soon as the session says something else.
#[test]
fn a_bell_means_needs_input_until_the_next_output() {
    let (dir, server) = tuned_server(BELL_ONLY);
    smol::block_on(async {
        let mut watcher = client(server.socket_path()).await;
        assert!(subscribe_and_snapshot(&mut watcher).await.is_empty());
        let mut owner = client(server.socket_path()).await;

        // Silence cannot reach the threshold on this server, so nothing but a
        // bell can produce a `NeedsInput` here.
        let session = raw_cat_session(&server, &mut owner, dir.path()).await;
        write_to(&mut owner, &session.id, b"\x07").await;
        status_becomes(&mut watcher, &session.id, SessionStatus::NeedsInput).await;

        write_to(&mut owner, &session.id, b"still here\n").await;
        assert_eq!(
            next_status(&mut watcher, &session.id).await,
            SessionStatus::Working,
            "the next output clears the bell"
        );

        kill(&mut owner, &session.id).await;
    });
}

/// Every subscriber is served the same events, and losing one is invisible to
/// the others.
#[test]
fn two_subscribers_see_the_same_events_and_are_independent() {
    let (dir, server) = fast_server();
    smol::block_on(async {
        let mut first = client(server.socket_path()).await;
        let mut second = client(server.socket_path()).await;
        assert!(subscribe_and_snapshot(&mut first).await.is_empty());
        assert!(subscribe_and_snapshot(&mut second).await.is_empty());

        let mut owner = client(server.socket_path()).await;
        let shared = create(&mut owner, dir.path(), "sleep 300").await;
        for watcher in [&mut first, &mut second] {
            let seen = event_until(watcher, "Created", |frame| match frame {
                Frame::Created { session, .. } => Some(session.id.clone()),
                _ => None,
            })
            .await;
            assert_eq!(seen, shared.id);
        }

        // Dropping a subscriber is an implicit unsubscribe and nothing more.
        drop(second);
        let later = create(&mut owner, dir.path(), "sleep 300").await;
        let seen = event_until(&mut first, "Created", |frame| match frame {
            Frame::Created { session, .. } => Some(session.id.clone()),
            _ => None,
        })
        .await;
        assert_eq!(seen, later.id);

        kill(&mut owner, &shared.id).await;
        event_until(&mut first, "Removed", |frame| match frame {
            Frame::Removed { session_id } if session_id == &shared.id => Some(()),
            _ => None,
        })
        .await;
        kill(&mut owner, &later.id).await;
    });
}

/// The idle rule, end to end: a session sitting at a shell prompt is `Idle`
/// rather than `NeedsInput`. Linux-only because the probe reads `/proc`.
#[cfg(target_os = "linux")]
#[test]
fn a_session_sitting_at_a_shell_prompt_reports_idle() {
    let (dir, server) = fast_server();
    smol::block_on(async {
        let mut watcher = client(server.socket_path()).await;
        assert!(subscribe_and_snapshot(&mut watcher).await.is_empty());
        let mut owner = client(server.socket_path()).await;

        let session = create(&mut owner, dir.path(), "sh -i").await;
        status_becomes(&mut watcher, &session.id, SessionStatus::Idle).await;
        assert_eq!(
            status_of(&list(&mut owner).await, &session.id),
            SessionStatus::Idle
        );

        kill(&mut owner, &session.id).await;
    });
}

/// Everything the upgrade decision reads out of the handshake: the daemon
/// names the exact bytes it runs (in tests, the test binary) and says whether
/// an exit would lose anything.
#[test]
fn the_handshake_reports_binary_identity_and_readiness() {
    let (dir, server) = server();
    smol::block_on(async {
        let stream = UnixStream::connect(server.socket_path())
            .await
            .expect("connecting");
        let mut connection = Connection::new(stream);
        let ack = connection
            .handshake(Hello::current())
            .await
            .expect("handshake");

        let hash = ack.binary_hash.expect("a daemon that can read its own exe");
        assert_eq!(hash.len(), 64, "hex sha256, got {hash:?}");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            ack.upgrade_ready,
            Some(true),
            "an empty table loses nothing"
        );

        // A live session flips readiness for the next handshake: `sleep` is not
        // a shell, so it never derives Idle and is never expendable.
        create(&mut connection, dir.path(), "sleep 30").await;
        let stream = UnixStream::connect(server.socket_path())
            .await
            .expect("connecting again");
        let ack = Connection::new(stream)
            .handshake(Hello::current())
            .await
            .expect("second handshake");
        assert_eq!(ack.binary_hash, Some(hash), "the hash does not move");
        assert_eq!(ack.upgrade_ready, Some(false));
    });
}

/// The load-bearing refusal: a daemon holding anything live answers Shutdown
/// with an error and keeps serving.
#[test]
fn shutdown_is_declined_while_anything_lives() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        create(&mut connection, dir.path(), "sleep 30").await;

        connection
            .send(&Frame::Shutdown {
                force: false,
                request_id: Some(40),
            })
            .await
            .expect("sending Shutdown");
        match recv(&mut connection, "the refusal").await {
            Frame::Error {
                code,
                message,
                request_id,
                ..
            } => {
                assert_eq!(request_id, Some(40));
                // Understood and not honoured, which is not the same answer as
                // "failed" and the client acts on it differently: wait.
                assert_eq!(code, error_code::DECLINED);
                assert!(message.contains("declined"), "got {message:?}");
            }
            other => panic!("expected Error, got {other:?}"),
        }

        // Still serving, still holding the session.
        assert_eq!(list(&mut connection).await.len(), 1);
        assert!(!server.was_shutdown());
    });
}

/// A polite shutdown and a state-creating request are one decision. Existing
/// connections can race after the socket is accepted, so unlinking it is not
/// enough: either the create lands and shutdown is declined, or shutdown wins
/// and the create is refused. Acknowledging both loses the new pty on exit.
#[test]
fn non_forced_shutdown_and_create_cannot_both_succeed() {
    for attempt in 0..4 {
        let (dir, server) = server();
        let socket = server.socket_path().to_owned();
        let gate = Arc::new(Barrier::new(2));
        let (sent, received) = std::sync::mpsc::channel();

        let shutdown = {
            let socket = socket.clone();
            let gate = gate.clone();
            let sent = sent.clone();
            std::thread::spawn(move || {
                smol::block_on(async move {
                    let mut connection = client(&socket).await;
                    gate.wait();
                    connection
                        .send(&Frame::Shutdown {
                            force: false,
                            request_id: Some(90),
                        })
                        .await
                        .expect("sending Shutdown");
                    let accepted = matches!(
                        connection.recv().await,
                        Ok(Frame::ShutdownAck {
                            request_id: Some(90)
                        })
                    );
                    sent.send(("shutdown", accepted))
                        .expect("reporting shutdown");
                });
            })
        };
        let create = {
            let socket = socket.clone();
            let cwd = dir.path().to_owned();
            let gate = gate.clone();
            let sent = sent.clone();
            std::thread::spawn(move || {
                smol::block_on(async move {
                    let mut connection = client(&socket).await;
                    gate.wait();
                    connection
                        .send(&create_frame(&cwd, "sleep 300", 91, None))
                        .await
                        .expect("sending CreateSession");
                    let accepted = matches!(
                        connection.recv().await,
                        Ok(Frame::Created {
                            request_id: Some(91),
                            ..
                        })
                    );
                    sent.send(("create", accepted)).expect("reporting create");
                });
            })
        };
        drop(sent);
        shutdown.join().expect("the shutdown racer");
        create.join().expect("the create racer");

        let outcomes: std::collections::HashMap<_, _> = received.into_iter().collect();
        assert!(
            !(outcomes["shutdown"] && outcomes["create"]),
            "attempt {attempt}: shutdown and create were both acknowledged"
        );
    }
}

/// A second connection that has *worked* here â€” touched a session, even one
/// it since killed â€” no longer blocks a shutdown over an empty table.
///
/// The gate is about the table, not about who is connected: Shutdown is
/// reached by a human clicking "upgrade host daemon" in an app that is itself
/// connected and busy with this daemon, so counting busy clients would decline
/// the one request the operator actually asked for.
#[test]
fn shutdown_is_accepted_even_while_another_client_is_busy() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut asking = client(server.socket_path()).await;
        let mut bystander = client(server.socket_path()).await;

        // Create and kill: the table is empty again, but the bystander's
        // connection has touched it and stays "busy" until it closes.
        let session = create(&mut bystander, dir.path(), "sleep 30").await;
        bystander
            .send(&Frame::Kill {
                session_id: session.id.clone(),
                request_id: Some(5),
            })
            .await
            .expect("sending Kill");
        match recv(&mut bystander, "the removal").await {
            Frame::Removed { session_id } => assert_eq!(session_id, session.id),
            other => panic!("expected Removed, got {other:?}"),
        }

        asking
            .send(&Frame::Shutdown {
                force: false,
                request_id: Some(41),
            })
            .await
            .expect("sending Shutdown");
        match recv(&mut asking, "the ack").await {
            Frame::ShutdownAck { request_id } => assert_eq!(request_id, Some(41)),
            other => panic!("expected ShutdownAck, got {other:?}"),
        }
    });
}

/// The one row an upgrade may never sacrifice: a session whose child is gone
/// but which nobody has killed. Its screen and scrollback exist only in this
/// process, and they are the evidence of how the agent died â€” so it blocks a
/// shutdown exactly the way a live session does, and readiness says so.
#[test]
fn shutdown_is_declined_over_an_exited_but_unkilled_session() {
    let (dir, server) = fast_server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let session = create(&mut connection, dir.path(), "sh -c 'exit 3'").await;
        wait_for(&mut connection, "the child to be reaped", |sessions| {
            status_of(sessions, &session.id) == SessionStatus::Exited
        })
        .await;

        connection
            .send(&Frame::Shutdown {
                force: false,
                request_id: Some(44),
            })
            .await
            .expect("sending Shutdown");
        match recv(&mut connection, "the refusal").await {
            Frame::Error { code, message, .. } => {
                assert_eq!(code, error_code::DECLINED);
                assert!(message.contains("declined"), "got {message:?}");
                assert!(message.contains("last screen"), "got {message:?}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(!server.was_shutdown());

        // And the handshake agrees, which is what keeps a client from even
        // asking.
        let stream = UnixStream::connect(server.socket_path())
            .await
            .expect("connecting");
        let ack = Connection::new(stream)
            .handshake(Hello::current())
            .await
            .expect("handshake");
        assert_eq!(ack.upgrade_ready, Some(false));
    });
}

/// The override: the same daemon that just declined a polite Shutdown accepts
/// a forced one over the very session it was protecting.
///
/// `force` is set only by a human clicking "upgrade host daemon", and the click
/// is the consent — the operator is owed a way past a daemon that would
/// otherwise be pinned to a stale binary by whatever happens to be running.
/// The sessions were persisted before they died with the process, so they come
/// back under the replacement daemon as lost rows the client recreates.
#[test]
fn a_forced_shutdown_is_accepted_over_a_live_session() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        create(&mut connection, dir.path(), "sleep 30").await;

        // Politely: declined, exactly as it is without the flag.
        connection
            .send(&Frame::Shutdown {
                force: false,
                request_id: Some(45),
            })
            .await
            .expect("sending Shutdown");
        match recv(&mut connection, "the refusal").await {
            Frame::Error { code, message, .. } => {
                assert_eq!(code, error_code::DECLINED);
                assert!(message.contains("declined"), "got {message:?}")
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(!server.was_shutdown());

        // Forced: the same table, the same session, and it exits anyway.
        connection
            .send(&Frame::Shutdown {
                force: true,
                request_id: Some(46),
            })
            .await
            .expect("sending forced Shutdown");
        match recv(&mut connection, "the ack").await {
            Frame::ShutdownAck { request_id } => assert_eq!(request_id, Some(46)),
            other => panic!("expected ShutdownAck, got {other:?}"),
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        while !server.was_shutdown() {
            assert!(Instant::now() < deadline, "the daemon never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
    });
}

/// A connection that only ever subscribed — the shape of a status stream, or
/// of a socket leaked by a dead client — never blocks a shutdown: it loses
/// nothing in a swap and reconnects to the replacement daemon on its own.
#[test]
fn a_merely_subscribed_client_never_blocks_shutdown() {
    let (_dir, server) = server();
    smol::block_on(async {
        let mut asking = client(server.socket_path()).await;
        let mut bystander = client(server.socket_path()).await;
        subscribe(&mut bystander).await;
        assert!(list(&mut bystander).await.is_empty());

        asking
            .send(&Frame::Shutdown {
                force: false,
                request_id: Some(43),
            })
            .await
            .expect("sending Shutdown");
        match recv(&mut asking, "the ack").await {
            Frame::ShutdownAck { request_id } => assert_eq!(request_id, Some(43)),
            other => panic!("expected ShutdownAck, got {other:?}"),
        }
    });
}

/// The accepted path: sole client, nothing but an empty table â€” ack on the
/// wire, socket unlinked, daemon "exited" (a flag in-process).
#[test]
fn shutdown_is_accepted_when_nothing_would_be_lost() {
    let (_dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;

        connection
            .send(&Frame::Shutdown {
                force: false,
                request_id: Some(42),
            })
            .await
            .expect("sending Shutdown");
        match recv(&mut connection, "the ack").await {
            Frame::ShutdownAck { request_id } => assert_eq!(request_id, Some(42)),
            other => panic!("expected ShutdownAck, got {other:?}"),
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        while !server.was_shutdown() {
            assert!(Instant::now() < deadline, "the daemon never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !server.socket_path().exists(),
            "the socket must be unlinked before anything can connect to a dead daemon"
        );
    });
}

/// Idle exit, end to end: a daemon serving nobody and holding nothing ends
/// itself after the configured grace.
#[test]
fn an_idle_daemon_exits_on_its_own() {
    let dir = TempDir::new().expect("temp dir");
    let config = ServerConfig::new(dir.path().join("daemon.sock"), dir.path().join("state"))
        .with_idle_exit(Some(Duration::from_millis(50)));
    let server = Server::spawn(config).expect("spawning server");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !server.was_shutdown() {
        assert!(Instant::now() < deadline, "the daemon never idle-exited");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!server.socket_path().exists());
}

/// The other half of the idle rule: a live session keeps the daemon alive with
/// no client anywhere near it â€” that is the whole point of the daemon.
#[test]
fn an_idle_timer_never_fires_over_a_live_session() {
    let (dir, server) = {
        let dir = TempDir::new().expect("temp dir");
        let config = ServerConfig::new(dir.path().join("daemon.sock"), dir.path().join("state"))
            .with_idle_exit(Some(Duration::from_millis(50)));
        let server = Server::spawn(config).expect("spawning server");
        (dir, server)
    };
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        create(&mut connection, dir.path(), "sleep 30").await;
    });
    // The connection is gone; the session is not. Ten grace periods pass.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !server.was_shutdown(),
        "a live session was abandoned by its daemon"
    );
    assert!(server.socket_path().exists());
}
