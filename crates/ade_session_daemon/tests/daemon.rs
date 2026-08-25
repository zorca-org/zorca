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

/// The name the shared workspace every plain [`create`] puts its session in is
/// made under. Sessions exist only inside a record now, so the helpers make one
/// rather than naming an id out of thin air.
const SHARED_WORKSPACE: &str = "ws-1";

fn create_frame(
    workspace_id: &str,
    cwd: &Path,
    command: &str,
    request_id: u64,
    scrollback: Option<u64>,
) -> Frame {
    Frame::CreateSession {
        workspace_id: workspace_id.to_owned(),
        cwd: cwd.display().to_string(),
        project_id: None,
        project_identity: None,
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

/// This server's shared workspace, made on first use. Which record a session
/// sits in is not what most of these tests are about, but it has to be a real
/// one — the daemon refuses a session in a workspace it does not hold.
async fn shared_workspace(connection: &mut Connection<UnixStream>, root: &Path) -> String {
    if let Some(existing) = list_workspaces(connection)
        .await
        .into_iter()
        .find(|workspace| workspace.name == SHARED_WORKSPACE)
    {
        return existing.id;
    }
    empty_workspace(connection, root, Some(SHARED_WORKSPACE))
        .await
        .id
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
    let workspace_id = shared_workspace(connection, cwd).await;
    create_in(connection, &workspace_id, cwd, command, scrollback).await
}

async fn create_in(
    connection: &mut Connection<UnixStream>,
    workspace_id: &str,
    cwd: &Path,
    command: &str,
    scrollback: Option<u64>,
) -> SessionInfo {
    connection
        .send(&create_frame(workspace_id, cwd, command, 7, scrollback))
        .await
        .expect("sending CreateSession");
    // By correlation id: a subscribed connection is told about its own new
    // session as an event too, ahead of its reply.
    event_until(connection, "the CreateSession reply", |frame| match frame {
        Frame::Created {
            session,
            request_id: Some(7),
            ..
        } => Some(session.clone()),
        Frame::Error {
            message,
            request_id: Some(7),
            ..
        } => panic!("CreateSession failed: {message}"),
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
            view_id: None,
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

/// `CreateWorkspace`: the record alone, which is all the op does. A workspace
/// with no sessions is a normal state, so the reply's `sessions` is always
/// empty.
async fn empty_workspace(
    connection: &mut Connection<UnixStream>,
    root: &Path,
    name: Option<&str>,
) -> WorkspaceInfo {
    connection
        .send(&Frame::CreateWorkspace {
            root: root.display().to_string(),
            name: name.map(str::to_owned),
            project_id: None,
            project_identity: None,
            request_id: Some(61),
            env: Vec::new(),
            cols: None,
            rows: None,
        })
        .await
        .expect("sending CreateWorkspace");
    // By correlation id, not by frame kind: a subscribed client is also told
    // about its own new workspace, ahead of its reply.
    let (workspace, sessions) =
        event_until(
            connection,
            "the CreateWorkspace reply",
            |frame| match frame {
                Frame::Workspace {
                    workspace,
                    sessions,
                    request_id: Some(61),
                    ..
                } => Some((workspace.clone(), sessions.clone())),
                Frame::Error { message, .. } => panic!("CreateWorkspace failed: {message}"),
                _ => None,
            },
        )
        .await;
    assert!(sessions.is_empty(), "an empty create spawned {sessions:?}");
    workspace
}

/// The client's "add a workspace" gesture, which is three ops now that the
/// daemon's combined create is gone: make the record, put the first login shell
/// in it, then write the one-leaf layout holding its tab. Returned as the
/// daemon now holds it, so the tests downstream of it are unchanged.
async fn create_workspace(
    connection: &mut Connection<UnixStream>,
    root: &Path,
    name: Option<&str>,
) -> (WorkspaceInfo, Vec<SessionInfo>) {
    let mut workspace = empty_workspace(connection, root, name).await;
    let session = create_in(connection, &workspace.id, root, "", None).await;
    let layout = LayoutDoc::single_terminal(session.id.clone());
    match update_layout(connection, &workspace.id, layout.clone(), 1).await {
        Frame::LayoutChanged { .. } => {}
        other => panic!("the first layout write was refused: {other:?}"),
    }
    workspace.layout = layout;
    workspace.layout_rev = 1;
    (workspace, vec![session])
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
            ..
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
        // Both in one workspace: `create` puts every session in the shared one.
        let first = create(&mut connection, dir.path(), CAT).await.id;
        let second = create(&mut connection, dir.path(), CAT).await.id;
        let shared = shared_workspace(&mut connection, dir.path()).await;
        let (workspace, _) = open_workspace(&mut connection, &shared).await;

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
        // `create` puts every session in the shared workspace, whose layout is
        // still the empty rev-0 one; this takes it to rev 1 with both tabs.
        let shared = shared_workspace(&mut connection, dir.path()).await;
        let (workspace, _) = open_workspace(&mut connection, &shared).await;
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
            // Rev 2: what a client holding the rev-1 document would send,
            // and what the scrub itself moves the document to.
            update_layout(&mut connection, &racing_id, stale, 2).await;
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
/// The kill wins outright now: nothing re-creates a record for a session, so
/// the create either lands wholly before the kill — and its session goes with
/// the workspace — or is refused. What can never be left behind is a session
/// claiming a workspace the daemon has announced as removed.
#[test]
fn killing_a_workspace_while_a_session_is_created_leaves_no_orphaned_session() {
    let (dir, server) = server();
    let socket = server.socket_path().to_owned();
    let doomed = smol::block_on(async {
        let mut connection = client(&socket).await;
        create(&mut connection, dir.path(), "sleep 300").await;
        shared_workspace(&mut connection, dir.path()).await
    });

    let cwd = dir.path().to_owned();
    let killed = doomed.clone();
    let raced = doomed.clone();
    race(
        &socket,
        move |socket, gate| {
            smol::block_on(async move {
                let mut connection = client(&socket).await;
                gate.wait();
                connection
                    .send(&Frame::KillWorkspace {
                        workspace_id: killed,
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
                // Either answer is legal; only the aftermath is asserted.
                connection
                    .send(&create_frame(&raced, &cwd, "sleep 300", 7, None))
                    .await
                    .expect("sending CreateSession");
                event_until(
                    &mut connection,
                    "the CreateSession reply",
                    |frame| match frame {
                        Frame::Created {
                            request_id: Some(7),
                            ..
                        }
                        | Frame::Error {
                            request_id: Some(7),
                            ..
                        } => Some(()),
                        _ => None,
                    },
                )
                .await;
            });
        },
    );

    smol::block_on(async {
        let mut connection = client(&socket).await;
        let claimants: Vec<SessionId> = list(&mut connection)
            .await
            .into_iter()
            .filter(|session| session.workspace_id == doomed)
            .map(|session| session.id)
            .collect();
        let recorded = list_workspaces(&mut connection)
            .await
            .iter()
            .any(|workspace| workspace.id == doomed);
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
        let shared = shared_workspace(&mut connection, dir.path()).await;
        let (workspace, _) = open_workspace(&mut connection, &shared).await;
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

/// The client's "add a workspace" gesture, end to end: the record, its login
/// shell, and a layout holding that shell's terminal tab. Three ops since
/// generation 3 retired the daemon's combined create — the state they leave is
/// what everything downstream of it still expects.
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

/// The op on its own: the record alone, which is what a panel row is before
/// anything is put in it. Nothing spawns and no layout is invented.
#[test]
fn creating_an_empty_workspace_returns_a_row_with_no_sessions() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let workspace = empty_workspace(&mut connection, dir.path(), Some("row")).await;

        assert_eq!(workspace.name, "row");
        assert!(workspace.layout.terminal_sessions().is_empty());
        // Zero, so the client's first layout write is revision 1.
        assert_eq!(workspace.layout_rev, 0);
        assert!(list(&mut connection).await.is_empty(), "something spawned");
        assert_eq!(
            list_workspaces(&mut connection).await,
            vec![workspace.clone()]
        );

        // A row with nothing in it is still killable: today's NOT_FOUND rule is
        // about a workspace nothing knows, not about an empty one.
        connection
            .send(&Frame::KillWorkspace {
                workspace_id: workspace.id.clone(),
                request_id: Some(67),
            })
            .await
            .expect("sending KillWorkspace");
        event_until(&mut connection, "WorkspaceRemoved", |frame| match frame {
            Frame::WorkspaceRemoved {
                workspace_id,
                request_id: Some(67),
                ..
            } if *workspace_id == workspace.id => Some(()),
            Frame::Error { message, .. } => panic!("KillWorkspace failed: {message}"),
            _ => None,
        })
        .await;
        assert!(list_workspaces(&mut connection).await.is_empty());
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
                project_id: None,
                project_identity: None,
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
        let (workspace, sessions) = create_workspace(&mut writer, dir.path(), None).await;
        // Subscribed after the gesture's own layout write, so the first
        // broadcast this observer sees is the one under test.
        subscribe(&mut observer).await;

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
                    ..
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
                ..
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
        assert!(
            ack.instance_id.is_some_and(|id| !id.is_empty()),
            "the daemon names itself, so two spellings of this host are one"
        );
    });
}

/// The identity is the state dir's, not the process's: a client that knows a
/// host by it must still know that host after the daemon restarts.
#[test]
fn the_instance_id_outlives_the_daemon() {
    async fn identity(socket: &Path) -> Option<String> {
        let stream = UnixStream::connect(socket).await.expect("connecting");
        Connection::new(stream)
            .handshake(Hello::current())
            .await
            .expect("handshake")
            .instance_id
    }

    let (dir, server) = server();
    let first = smol::block_on(identity(server.socket_path()));
    assert!(first.is_some());
    drop(server);

    let server = server_named(&dir, "daemon-2.sock");
    let second = smol::block_on(identity(server.socket_path()));
    assert_eq!(second, first);
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

/// The other read-only cause, and the one that used to be invisible: a ledger
/// that exists and did not parse. The daemon serves from an empty table, so a
/// client that took the flag for "everything is fine" would read that emptiness
/// as the truth about the host and delete its own rows on the strength of it.
#[test]
fn hello_reports_degraded_when_the_ledger_could_not_be_read() {
    let dir = TempDir::new().expect("temp dir");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).expect("the state dir");
    std::fs::write(state.join("sessions.json"), b"this was never json")
        .expect("seeding an unreadable ledger");

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
            "a ledger this daemon could not read is one it must not write over"
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
        assert_eq!(
            session.workspace_id,
            shared_workspace(&mut connection, dir.path()).await,
            "the session is in the record the client named, not one minted for it"
        );
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

/// An interactive shell gives its foreground job a process group of its own.
/// `Kill` must not acknowledge the shell while that job can still hold files
/// or locks needed by the replacement session.
#[test]
fn kill_waits_for_a_foreground_job_that_ignores_sighup() {
    let (dir, server) = server();
    let pid_file = dir.path().join("foreground.pid");
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let command = format!(
            "sh -c 'set -m; (trap \"\" HUP; while :; do sleep 1; done) & echo $! > {}; fg'",
            pid_file.display()
        );
        let session = create(&mut connection, dir.path(), &command).await;
        let pid = wait_for_pid(&pid_file).await;

        kill(&mut connection, &session.id).await;

        if running(pid) {
            // SAFETY: this process group was created by the command above.
            let cleanup = unsafe { libc::kill(-pid, libc::SIGKILL) };
            assert_eq!(cleanup, 0, "failed to clean up foreground job {pid}");
            panic!("Kill was acknowledged while the foreground job {pid} was still alive");
        }
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
/// The replay is history plus a repaint synthesized under the same lock that
/// fans out live output, so `alpha` appears exactly twice — raw and painted —
/// both before `beta`, which arrived after and appears once, live.
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
        assert_eq!(occurrences(&seen, b"alpha"), 2, "{seen:?}");
        assert_eq!(occurrences(&seen, b"beta"), 1, "{seen:?}");
        let alpha = seen
            .windows(5)
            .rposition(|window| window == b"alpha")
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
        // History replays the raw lines, and the repaint paints them again
        // with `CUP`: twice each, nothing more.
        assert_eq!(occurrences(&replayed, b"before"), 2, "{replayed:?}");
        assert_eq!(occurrences(&replayed, b"while-away"), 2, "{replayed:?}");

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
        // Resize carries no ack; a replied request on the same connection
        // proves the sequential request loop has applied it before the viewer
        // attaches from its own connection.
        let _ = list(&mut connection).await;

        let mut viewer = client(server.socket_path()).await;
        let (replayed, truncated) = attach(&mut viewer, &session.id).await;
        // Nothing may be painted onto a row the screen no longer has.
        assert!(
            !contains(&replayed, b"\x1b[9;1H"),
            "painted a ninth row onto an eight-row screen: {replayed:?}"
        );
        // The newest output is what survives a shrink, and the screen says so.
        assert!(contains(&replayed, b"line12"), "{replayed:?}");
        assert!(
            !truncated,
            "the scrolled-off lines are in the replayed history; nothing was omitted"
        );
    });
}

/// A terminal can attach before its real pane size is known. When that pane
/// later resizes, it must receive a repaint at the corrected size instead of
/// keeping the tiny initial screen until the child happens to redraw.
#[test]
fn resize_after_attach_repaints_at_the_new_size() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let session = cat_session(&server, &mut connection, dir.path()).await;
        for line in 1..=12 {
            write_to(
                &mut connection,
                &session.id,
                format!("line{line}\n").as_bytes(),
            )
            .await;
        }
        wait_for_ring(server.socket_path(), &session.id, b"line12").await;

        let mut viewer = client(server.socket_path()).await;
        let _ = attach(&mut viewer, &session.id).await;
        viewer
            .send(&Frame::Resize {
                session_id: session.id.clone(),
                cols: 80,
                rows: 8,
            })
            .await
            .expect("sending Resize");

        match recv(&mut viewer, "the resized repaint").await {
            Frame::Output { session_id, bytes } => {
                assert_eq!(session_id, session.id);
                assert!(contains(&bytes, b"\x1b[?2026h"), "{bytes:?}");
                assert!(!contains(&bytes, b"\x1b[9;1H"), "{bytes:?}");
                assert!(contains(&bytes, b"line12"), "{bytes:?}");
            }
            other => panic!("expected resized repaint, got {other:?}"),
        }
    });
}

#[test]
fn generation_two_resize_repaints_every_attached_viewer() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let session = cat_session(&server, &mut connection, dir.path()).await;
        write_to(&mut connection, &session.id, b"shared-screen\n").await;
        wait_for_ring(server.socket_path(), &session.id, b"shared-screen").await;

        let mut first = gen2_client(server.socket_path()).await;
        let mut second = gen2_client(server.socket_path()).await;
        let _ = attach(&mut first, &session.id).await;
        let _ = attach(&mut second, &session.id).await;

        first
            .send(&Frame::Resize {
                session_id: session.id.clone(),
                cols: 80,
                rows: 8,
            })
            .await
            .expect("sending generation-two Resize");

        for viewer in [&mut first, &mut second] {
            match recv(viewer, "the shared resized repaint").await {
                Frame::Output { session_id, bytes } => {
                    assert_eq!(session_id, session.id);
                    assert!(contains(&bytes, b"\x1b[?2026h"), "{bytes:?}");
                    assert!(contains(&bytes, b"shared-screen"), "{bytes:?}");
                }
                other => panic!("expected resized repaint, got {other:?}"),
            }
        }
    });
}

/// A focus claim that arrives before its view attaches is honored the moment
/// the view does: the repaint comes out at the focused ask, not at the
/// smallest sibling's ask that stood in for it while the claim was pending.
#[test]
fn a_focus_claim_resolved_by_attach_repaints_at_the_focused_ask() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let session = cat_session(&server, &mut connection, dir.path()).await;
        for line in 1..=12 {
            write_to(
                &mut connection,
                &session.id,
                format!("line{line}\n").as_bytes(),
            )
            .await;
        }
        wait_for_ring(server.socket_path(), &session.id, b"line12").await;
        // A small sibling holds the minimum down at eight rows.
        connection
            .send(&Frame::Resize {
                session_id: session.id.clone(),
                cols: 80,
                rows: 8,
            })
            .await
            .expect("sending Resize");
        let _ = list(&mut connection).await;

        // The viewer claims focus for its view and asks for a taller screen
        // before attaching; the claim can only resolve at the attach.
        let mut viewer = client(server.socket_path()).await;
        viewer
            .send(&Frame::FocusSession {
                session_id: session.id.clone(),
                view_id: "view-tall".to_owned(),
                hover: false,
            })
            .await
            .expect("sending FocusSession");
        viewer
            .send(&Frame::Resize {
                session_id: session.id.clone(),
                cols: 80,
                rows: 24,
            })
            .await
            .expect("sending Resize");
        let _ = list(&mut viewer).await;
        viewer
            .send(&Frame::Attach {
                session_id: session.id.clone(),
                view_id: Some("view-tall".to_owned()),
                request_id: Some(31),
            })
            .await
            .expect("sending Attach");
        match recv(&mut viewer, "Replay").await {
            Frame::Replay { bytes, .. } => assert!(
                contains(&bytes, b"\x1b[12;1H"),
                "repainted at the focused 24 rows, not the sibling's 8: {bytes:?}"
            ),
            other => panic!("expected Replay, got {other:?}"),
        }
    });
}

/// A view that takes focus after both clients attached receives the corrected
/// screen immediately, without waiting for another local resize.
#[test]
fn a_focus_change_repaints_the_new_owner() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut sibling = client(server.socket_path()).await;
        let session = cat_session(&server, &mut sibling, dir.path()).await;
        for line in 1..=12 {
            write_to(
                &mut sibling,
                &session.id,
                format!("line{line}\n").as_bytes(),
            )
            .await;
        }
        wait_for_ring(server.socket_path(), &session.id, b"line12").await;
        sibling
            .send(&Frame::Resize {
                session_id: session.id.clone(),
                cols: 80,
                rows: 8,
            })
            .await
            .expect("sending the sibling size");
        let _ = list(&mut sibling).await;

        let mut viewer = client(server.socket_path()).await;
        viewer
            .send(&Frame::Resize {
                session_id: session.id.clone(),
                cols: 80,
                rows: 24,
            })
            .await
            .expect("sending the viewer size");
        viewer
            .send(&Frame::Attach {
                session_id: session.id.clone(),
                view_id: Some("view-tall".to_owned()),
                request_id: Some(32),
            })
            .await
            .expect("sending Attach");
        match recv(&mut viewer, "Replay").await {
            Frame::Replay { bytes, .. } => assert!(
                !contains(&bytes, b"\x1b[12;1H"),
                "the unfocused viewer starts at the sibling's size: {bytes:?}"
            ),
            other => panic!("expected Replay, got {other:?}"),
        }

        viewer
            .send(&Frame::FocusSession {
                session_id: session.id.clone(),
                view_id: "view-tall".to_owned(),
                hover: false,
            })
            .await
            .expect("sending FocusSession");
        match recv(&mut viewer, "the focused repaint").await {
            Frame::Output { session_id, bytes } => {
                assert_eq!(session_id, session.id);
                assert!(contains(&bytes, b"\x1b[?2026h"), "{bytes:?}");
                assert!(contains(&bytes, b"\x1b[12;1H"), "{bytes:?}");
            }
            other => panic!("expected focused repaint, got {other:?}"),
        }
    });
}

#[test]
fn an_equal_size_focus_change_still_repaints_the_new_owner() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut sibling = client(server.socket_path()).await;
        let session = cat_session(&server, &mut sibling, dir.path()).await;
        sibling
            .send(&Frame::Write {
                session_id: session.id.clone(),
                bytes: b"\x1b[<35;1;1M\n".to_vec(),
            })
            .await
            .expect("sending a mouse report");
        wait_for_ring(server.socket_path(), &session.id, b"[<35;1;1M").await;

        let mut viewer = client(server.socket_path()).await;
        viewer
            .send(&Frame::Resize {
                session_id: session.id.clone(),
                cols: 80,
                rows: 24,
            })
            .await
            .expect("sending the viewer size");
        viewer
            .send(&Frame::Attach {
                session_id: session.id.clone(),
                view_id: Some("view-equal".to_owned()),
                request_id: Some(33),
            })
            .await
            .expect("attaching the viewer");
        let _ = recv(&mut viewer, "Replay").await;

        viewer
            .send(&Frame::FocusSession {
                session_id: session.id.clone(),
                view_id: "view-equal".to_owned(),
                hover: true,
            })
            .await
            .expect("focusing the equal-size viewer");
        match recv(&mut viewer, "the equal-size repaint").await {
            Frame::Output { session_id, bytes } => {
                assert_eq!(session_id, session.id);
                assert!(contains(&bytes, b"\x1b[?2026h"), "{bytes:?}");
            }
            other => panic!("expected focused repaint, got {other:?}"),
        }

        viewer
            .send(&Frame::FocusSession {
                session_id: session.id.clone(),
                view_id: "view-equal".to_owned(),
                hover: true,
            })
            .await
            .expect("hovering the owning viewer again");
        assert!(matches!(
            recv(&mut viewer, "the repeated hover repaint").await,
            Frame::Output { .. }
        ));
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

        // The app exits. The needle is the leave sequence itself, as cat
        // echoes it back — "primary-output" has been in the ring since before
        // the trip and would not wait for anything.
        write_to(&mut connection, &session.id, b"\x1b[?1049l\n").await;
        wait_for_ring(server.socket_path(), &session.id, b"\x1b[?1049l").await;

        let (after, _) = attach(&mut viewer, &session.id).await;
        // The history replays the app's whole alternate-screen trip, so the
        // client visits it again — what matters is that it is left again.
        assert_eq!(
            occurrences(&after, b"\x1b[?1049h"),
            occurrences(&after, b"\x1b[?1049l"),
            "an alternate screen was entered and never left: {after:?}"
        );
        let last_enter = after
            .windows(8)
            .rposition(|window| window == b"\x1b[?1049h")
            .expect("the history enters the alternate screen");
        let last_leave = after
            .windows(8)
            .rposition(|window| window == b"\x1b[?1049l")
            .expect("the history leaves the alternate screen");
        assert!(
            last_enter < last_leave,
            "back on the primary screen: {after:?}"
        );
        // Past the trip, only the primary screen is painted.
        let back = &after[last_leave..];
        assert!(
            contains(back, b"primary-output"),
            "the primary screen survived the app: {after:?}"
        );
        assert!(
            !contains(back, b"ALTSCREEN"),
            "and none of the app is left on it: {after:?}"
        );
    });
}

/// A session that printed more than fits keeps the newest of it and says so.
///
/// Once the ring wraps, attach reports truncation and still restores the
/// retained tail before repainting the visible screen.
#[test]
fn scrollback_wraps_to_the_tail_and_reports_truncation() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut connection = client(server.socket_path()).await;
        let session =
            create_with_scrollback(&mut connection, dir.path(), "seq 1 500", Some(512)).await;
        wait_for(&mut connection, "the session to exit", |sessions| {
            sessions.iter().any(|s| s.status == SessionStatus::Exited)
        })
        .await;

        let mut viewer = client(server.socket_path()).await;
        let (replayed, truncated) = replay_containing(&mut viewer, &session.id, b"500").await;
        assert!(truncated, "500 lines cannot fit in 512 bytes");
        assert!(
            contains(&replayed, b"450\r\n"),
            "recent history above the visible screen is retained: {replayed:?}"
        );
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
                ..
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

/// There is no implicit workspace any more: a `CreateSession` naming a record
/// the daemon does not hold is refused, and an empty id is a different refusal
/// because it is a different mistake. Nothing is spawned and nothing is
/// recorded either way — the wire-level half of the spec's "sessions are
/// created only inside an existing workspace".
#[test]
fn a_session_naming_a_workspace_the_daemon_does_not_hold_is_refused() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut watcher = client(server.socket_path()).await;
        assert!(subscribe_and_snapshot(&mut watcher).await.is_empty());

        let mut owner = client(server.socket_path()).await;
        for (workspace_id, expected) in [
            ("no-such-workspace", error_code::NOT_FOUND),
            ("", error_code::INVALID_ARGUMENT),
        ] {
            owner
                .send(&create_frame(workspace_id, dir.path(), CAT, 7, None))
                .await
                .expect("sending CreateSession");
            match recv(&mut owner, "the refusal").await {
                Frame::Error { code, message, .. } => {
                    assert_eq!(code, expected, "for {workspace_id:?}: {message}");
                }
                other => panic!("expected Error for {workspace_id:?}, got {other:?}"),
            }
        }

        assert!(
            list(&mut owner).await.is_empty(),
            "a refused create spawned"
        );
        assert!(
            list_workspaces(&mut owner).await.is_empty(),
            "a refused create minted a workspace"
        );
    });
}

/// A session event must never name a workspace the subscriber has not learned
/// about yet, and the client's two calls are what put them in that order: the
/// record is announced — empty, since that is all `CreateWorkspace` makes —
/// before any `Created` naming it.
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
            Ok(announced) => assert!(announced.is_empty(), "{announced:?}"),
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
        // Made ahead of the race: the create has to be refusable only by the
        // shutdown, never by a workspace that is not there.
        let workspace = smol::block_on(async {
            let mut connection = client(&socket).await;
            empty_workspace(&mut connection, dir.path(), Some(SHARED_WORKSPACE))
                .await
                .id
        });
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
            let workspace = workspace.clone();
            let gate = gate.clone();
            let sent = sent.clone();
            std::thread::spawn(move || {
                smol::block_on(async move {
                    let mut connection = client(&socket).await;
                    gate.wait();
                    connection
                        .send(&create_frame(&workspace, &cwd, "sleep 300", 91, None))
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

// ---- the previous generation, served ----

/// A peer pinned to the previous generation: what a build from the cut, and
/// every client made against one, advertises.
fn gen2_hello() -> Hello {
    Hello {
        min_generation: MIN_GENERATION,
        max_generation: MIN_GENERATION,
        capabilities: Vec::new(),
        request_id: Some(1),
    }
}

async fn gen2_client(socket: &Path) -> Connection<UnixStream> {
    let stream = UnixStream::connect(socket).await.expect("connecting");
    let mut connection = Connection::new(stream);
    let ack = connection.handshake(gen2_hello()).await.expect("handshake");
    assert_eq!(
        ack.generation, MIN_GENERATION,
        "a peer that can only speak the window's lower end must be served there"
    );
    connection
}

/// The env the combined create is asked to put in its first shell. Distinctive
/// enough that the echo of the command asking for it cannot be mistaken for the
/// expansion.
const COMBINED_ENV: &str = "combined-env-ok";

/// The reply to `request_id`, ignoring the events that overtake it on a
/// subscribed connection.
async fn reply_to(connection: &mut Connection<UnixStream>, request_id: u64, what: &str) -> Frame {
    event_until(connection, what, |frame| {
        (frame.request_id() == Some(request_id)).then(|| frame.clone())
    })
    .await
}

/// `create_workspace` as a client from the cut sends it, the three session
/// fields populated. The dialect is the connection's and never the frame's, so
/// this is also what a generation-3 connection must answer with the record
/// alone.
///
/// Returns the reply whatever it is: the refusal is half of what is pinned here.
async fn legacy_create_workspace(
    connection: &mut Connection<UnixStream>,
    root: &Path,
    name: Option<&str>,
    request_id: u64,
) -> Frame {
    connection
        .send(&Frame::CreateWorkspace {
            root: root.display().to_string(),
            name: name.map(str::to_owned),
            project_id: None,
            project_identity: None,
            env: vec![("ADE_COMBINED".to_owned(), COMBINED_ENV.to_owned())],
            cols: Some(120),
            rows: Some(40),
            request_id: Some(request_id),
        })
        .await
        .expect("sending CreateWorkspace");
    reply_to(connection, request_id, "the CreateWorkspace reply").await
}

/// An empty command, i.e. the login shell every generation-2 auto-create path
/// spawns.
async fn legacy_create_session(
    connection: &mut Connection<UnixStream>,
    workspace_id: &str,
    cwd: &Path,
    label: &str,
    request_id: u64,
) -> Frame {
    connection
        .send(&Frame::CreateSession {
            workspace_id: workspace_id.to_owned(),
            cwd: cwd.display().to_string(),
            project_id: None,
            project_identity: None,
            command: String::new(),
            env: Vec::new(),
            cols: 90,
            rows: 25,
            agent_kind: "shell".to_owned(),
            instance_label: label.to_owned(),
            scrollback_bytes: None,
            request_id: Some(request_id),
        })
        .await
        .expect("sending CreateSession");
    reply_to(connection, request_id, "the CreateSession reply").await
}

fn workspace_named<'a>(workspaces: &'a [WorkspaceInfo], id: &str) -> Option<&'a WorkspaceInfo> {
    workspaces.iter().find(|workspace| workspace.id == id)
}

/// A directory that exists, for a create whose shell has to start in it.
fn project_root(dir: &TempDir, name: &str) -> PathBuf {
    let root = dir.path().join(name);
    std::fs::create_dir_all(&root).expect("the project root");
    root
}

/// A root the daemon's own user cannot enter, i.e. a create whose shell cannot
/// start.
///
/// It has to *be* a directory: `portable-pty` drops a cwd that is not one and
/// silently starts the shell in `$HOME` instead, so a path that merely does not
/// exist spawns happily. Readable, so the temp dir can still be swept up. Root
/// enters it anyway, and the tests using it then fail loudly rather than pass.
fn unenterable_root(dir: &TempDir, name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let root = project_root(dir, name);
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o600))
        .expect("barring the project root");
    root
}

/// At generation 2 `create_workspace` is the combined create: the record, one
/// login shell at the size and env the request named, and the one-leaf layout
/// holding it. Breaking this leaves every client from the cut with a workspace
/// that never opens a terminal.
#[test]
fn a_generation_two_peer_gets_the_combined_workspace_create() {
    let (dir, server) = server();
    let root = project_root(&dir, "proj");
    smol::block_on(async {
        let mut daemon = gen2_client(server.socket_path()).await;
        let (workspace, sessions) =
            match legacy_create_workspace(&mut daemon, &root, Some("proj"), 71).await {
                Frame::Workspace {
                    workspace,
                    sessions,
                    ..
                } => (workspace, sessions),
                other => panic!("expected a workspace, got {other:?}"),
            };
        assert_eq!(sessions.len(), 1, "the combined create makes one session");
        assert_eq!(
            workspace.layout.terminal_sessions(),
            vec![sessions[0].id.clone()],
            "the one-leaf layout must hold that session"
        );
        assert_eq!(workspace.name, "proj");
        assert_eq!(workspace.project_root, root.display().to_string());
        assert_eq!(workspace.layout_rev, 1, "the create's own layout write");

        let listed = list(&mut daemon).await;
        assert_eq!(listed.len(), 1, "one shell, neither none nor two");
        assert_eq!(listed[0].id, sessions[0].id);

        // The legacy request fields reach the pty and not only the record: a
        // shell at 80x24 without the env is the silent half of this regression.
        attach(&mut daemon, &sessions[0].id).await;
        write_to(
            &mut daemon,
            &sessions[0].id,
            b"printf '[%s]\\n' \"$ADE_COMBINED\"; stty size\n",
        )
        .await;
        let seen = output_until(&mut daemon, &sessions[0].id, b"40 120").await;
        assert!(
            contains(&seen, format!("[{COMBINED_ENV}]").as_bytes()),
            "the request's env never reached the shell: {}",
            String::from_utf8_lossy(&seen)
        );
    });
}

/// The other half of the same op: at generation 3 the record is all there is,
/// and the legacy request fields mean nothing.
#[test]
fn a_generation_three_peer_gets_the_record_alone() {
    let (dir, server) = server();
    let root = project_root(&dir, "proj");
    smol::block_on(async {
        let mut daemon = client(server.socket_path()).await;
        let workspace = match legacy_create_workspace(&mut daemon, &root, Some("proj"), 72).await {
            Frame::Workspace {
                workspace,
                sessions,
                ..
            } => {
                assert!(sessions.is_empty(), "generation 3 spawns nothing");
                workspace
            }
            other => panic!("expected a workspace, got {other:?}"),
        };
        assert_eq!(workspace.layout, LayoutDoc::empty());
        assert_eq!(workspace.layout_rev, 0, "nothing has written a layout yet");
        assert!(
            list(&mut daemon).await.is_empty(),
            "a gen-3 create_workspace spawned a terminal"
        );
    });
}

/// Generation 2 never refused a session for want of a workspace — it made one,
/// under the id the client named or a fresh one when it named none. A refusal
/// here is a client from the cut that can no longer open a terminal at all.
#[test]
fn a_generation_two_create_session_makes_the_workspace_it_names() {
    let (dir, server) = server();
    let cwd = project_root(&dir, "from-the-cut");
    smol::block_on(async {
        let mut daemon = gen2_client(server.socket_path()).await;

        let named =
            match legacy_create_session(&mut daemon, "ws-from-the-cut", &cwd, "labelled", 73).await
            {
                Frame::Created { session, .. } => session,
                other => panic!("expected the session, got {other:?}"),
            };
        assert_eq!(
            named.workspace_id, "ws-from-the-cut",
            "auto-create must keep the id the old client already named"
        );

        let minted = match legacy_create_session(&mut daemon, "", &cwd, "", 74).await {
            Frame::Created { session, .. } => session,
            other => panic!("expected the session, got {other:?}"),
        };
        assert!(
            !minted.workspace_id.is_empty() && minted.workspace_id != named.workspace_id,
            "an empty id must mint a fresh workspace, got {:?}",
            minted.workspace_id
        );

        let workspaces = list_workspaces(&mut daemon).await;
        assert_eq!(workspaces.len(), 2, "both auto-creates left a record");
        for (session, expected_name) in [(&named, "labelled"), (&minted, "from-the-cut")] {
            let workspace = workspace_named(&workspaces, &session.workspace_id)
                .expect("the auto-created record");
            assert_eq!(workspace.name, expected_name);
            assert_eq!(workspace.project_root, cwd.display().to_string());
            assert_eq!(workspace.project_scope_rev, 0);
            assert_eq!(
                workspace.layout.terminal_sessions(),
                vec![session.id.clone()],
                "the auto-created record must hold its session in a one-leaf layout"
            );
        }
    });
}

/// `focus_session` is a generation-3 operation. A gen-2 connection is told so
/// and keeps serving — refusing the request, not the stream, which is the
/// difference between one lost gesture and a client that has to reconnect.
#[test]
fn focus_session_is_refused_at_generation_two_without_ending_the_connection() {
    let (_dir, server) = server();
    smol::block_on(async {
        let mut daemon = gen2_client(server.socket_path()).await;
        daemon
            .send(&Frame::FocusSession {
                session_id: SessionId::new("s-1"),
                view_id: "view-1".to_owned(),
                hover: false,
            })
            .await
            .expect("sending FocusSession");
        match recv(&mut daemon, "the refusal").await {
            Frame::Error { code, .. } => assert_eq!(code, error_code::UNCAPABLE_PEER),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(
            list(&mut daemon).await.is_empty(),
            "the refusal cost one request and nothing else"
        );
    });
}

/// §4.2's composed create, in order: the record appears before it holds
/// anything, and the layout arrives last. A subscriber that assumes a workspace
/// is complete the moment it first sees it is what this order breaks.
#[test]
fn the_generation_two_combined_create_publishes_its_steps_in_order() {
    let (dir, server) = server();
    let root = project_root(&dir, "stepwise");
    smol::block_on(async {
        let mut observer = client(server.socket_path()).await;
        subscribe(&mut observer).await;
        let mut daemon = gen2_client(server.socket_path()).await;

        let (workspace, sessions) =
            match legacy_create_workspace(&mut daemon, &root, Some("stepwise"), 75).await {
                Frame::Workspace {
                    workspace,
                    sessions,
                    ..
                } => (workspace, sessions),
                other => panic!("expected a workspace, got {other:?}"),
            };
        let session = sessions[0].id.clone();

        let deadline = Instant::now() + Duration::from_secs(45);
        let mut steps: Vec<&str> = Vec::new();
        loop {
            match recv(&mut observer, "the combined create's events").await {
                Frame::Workspace {
                    workspace: seen, ..
                } if seen.id == workspace.id => {
                    assert_eq!(seen.layout, LayoutDoc::empty());
                    assert_eq!(seen.layout_rev, 0);
                    steps.push("workspace");
                }
                Frame::Created { session: seen, .. } if seen.id == session => {
                    steps.push("created");
                }
                Frame::LayoutChanged {
                    workspace_id,
                    layout,
                    rev,
                    ..
                } if workspace_id == workspace.id => {
                    assert_eq!(rev, 1);
                    assert_eq!(layout.terminal_sessions(), vec![session.clone()]);
                    steps.push("layout");
                    break;
                }
                _ => {}
            }
            assert!(
                Instant::now() < deadline,
                "the create's layout event never arrived, saw {steps:?}"
            );
        }
        assert_eq!(steps, ["workspace", "created", "layout"]);
    });
}

/// The failing half of §4.2's composed create: a subscriber sees the record
/// appear and then vanish, and nothing is left behind to become an empty row
/// nobody owns.
#[test]
fn a_failed_generation_two_combined_create_publishes_the_removal_it_caused() {
    let (dir, server) = server();
    let gone = unenterable_root(&dir, "gone");
    smol::block_on(async {
        let mut observer = client(server.socket_path()).await;
        subscribe(&mut observer).await;
        let mut daemon = gen2_client(server.socket_path()).await;

        let workspace_id = match legacy_create_workspace(&mut daemon, &gone, Some("gone"), 76).await
        {
            Frame::Error { workspace_id, .. } => {
                workspace_id.expect("the refusal names the record it took back")
            }
            other => panic!("expected a refusal, got {other:?}"),
        };

        let deadline = Instant::now() + Duration::from_secs(45);
        let mut steps: Vec<&str> = Vec::new();
        loop {
            match recv(&mut observer, "the failed create's events").await {
                Frame::Workspace { workspace, .. } if workspace.id == workspace_id => {
                    steps.push("workspace");
                }
                Frame::WorkspaceRemoved {
                    workspace_id: removed,
                    ..
                } if removed == workspace_id => {
                    steps.push("removed");
                    break;
                }
                _ => {}
            }
            assert!(
                Instant::now() < deadline,
                "the compensating removal never arrived, saw {steps:?}"
            );
        }
        assert_eq!(steps, ["workspace", "removed"]);

        let workspaces = list_workspaces(&mut daemon).await;
        assert!(
            workspace_named(&workspaces, &workspace_id).is_none(),
            "the record outlived the create that failed: {workspaces:?}"
        );
    });
}

/// §4.2: a name is trimmed, and a blank one falls back to the root's basename,
/// at both generations. A row the user cannot tell from another is not a name.
#[test]
fn workspace_names_are_trimmed_and_fall_back_to_the_root_basename() {
    let (dir, server) = server();
    let root = project_root(&dir, "named-root");
    smol::block_on(async {
        // The gen-2 arm spawns a shell per create, so the root has to be real.
        let mut connections = [
            ("generation 2", gen2_client(server.socket_path()).await),
            ("generation 3", client(server.socket_path()).await),
        ];
        let mut request_id = 80;
        for (generation, daemon) in &mut connections {
            for (given, expected) in [
                (Some("  project  "), "project"),
                (Some("   "), "named-root"),
                (None, "named-root"),
            ] {
                request_id += 1;
                match legacy_create_workspace(daemon, &root, given, request_id).await {
                    Frame::Workspace { workspace, .. } => assert_eq!(
                        workspace.name, expected,
                        "{generation} stored {given:?} verbatim"
                    ),
                    other => panic!("expected a workspace, got {other:?}"),
                }
            }
        }
    });
}

/// A create that cannot be completed takes back the record it made, and a retry
/// takes back its own — or a client hammering a dead cwd fills the panel with
/// empty rows.
#[test]
fn a_generation_two_auto_create_leaves_no_workspace_when_the_spawn_fails() {
    let (dir, server) = server();
    let barred = unenterable_root(&dir, "barred");
    smol::block_on(async {
        let mut daemon = gen2_client(server.socket_path()).await;

        for request_id in [90, 91] {
            match legacy_create_session(&mut daemon, "ws-doomed", &barred, "doomed", request_id)
                .await
            {
                Frame::Error { .. } => {}
                other => panic!("a shell cannot start in {barred:?}, got {other:?}"),
            }
            let workspaces = list_workspaces(&mut daemon).await;
            assert!(
                workspace_named(&workspaces, "ws-doomed").is_none(),
                "attempt {request_id} left its auto-created record behind: {workspaces:?}"
            );
        }

        // The minting form compensates the same way, and has no id to look for.
        let before = list_workspaces(&mut daemon).await.len();
        match legacy_create_session(&mut daemon, "", &barred, "", 92).await {
            Frame::Error { .. } => {}
            other => panic!("a shell cannot start in {barred:?}, got {other:?}"),
        }
        assert_eq!(
            list_workspaces(&mut daemon).await.len(),
            before,
            "a minted workspace outlived the create that failed"
        );
    });
}

/// Compensation is scoped to what the request made: a create that fails over a
/// workspace it merely reused must not take that workspace, and the sessions in
/// it, with it.
#[test]
fn a_reused_auto_create_record_survives_a_later_failed_create() {
    let (dir, server) = server();
    let cwd = project_root(&dir, "shared");
    let barred = unenterable_root(&dir, "barred");
    smol::block_on(async {
        let mut daemon = gen2_client(server.socket_path()).await;

        let session =
            match legacy_create_session(&mut daemon, "ws-shared", &cwd, "shared", 95).await {
                Frame::Created { session, .. } => session,
                other => panic!("expected the session, got {other:?}"),
            };
        match legacy_create_session(&mut daemon, "ws-shared", &barred, "shared", 96).await {
            Frame::Error { .. } => {}
            other => panic!("a shell cannot start in {barred:?}, got {other:?}"),
        }

        let workspaces = list_workspaces(&mut daemon).await;
        let workspace = workspace_named(&workspaces, "ws-shared")
            .expect("the failed create took a workspace it did not make");
        assert_eq!(
            workspace.layout.terminal_sessions(),
            vec![session.id.clone()],
            "the survivor lost the layout naming its first session"
        );
        let sessions = list(&mut daemon).await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session.id);
    });
}

/// §4.2's one layout write that does not exclude its writer: `Created` carries
/// no layout, so a gen-2 client subscribed on the connection it creates from
/// would otherwise believe its own workspace is still empty. Every other layout
/// write excludes the connection that asked for it.
#[test]
fn the_requesting_connection_learns_the_auto_created_layout() {
    let (dir, server) = server();
    let cwd = project_root(&dir, "self-taught");
    smol::block_on(async {
        let mut daemon = gen2_client(server.socket_path()).await;
        subscribe(&mut daemon).await;
        daemon
            .send(&Frame::CreateSession {
                workspace_id: "ws-self-taught".to_owned(),
                cwd: cwd.display().to_string(),
                project_id: None,
                project_identity: None,
                command: String::new(),
                env: Vec::new(),
                cols: 90,
                rows: 25,
                agent_kind: "shell".to_owned(),
                instance_label: "self-taught".to_owned(),
                scrollback_bytes: None,
                request_id: Some(97),
            })
            .await
            .expect("sending CreateSession");

        // The event precedes the reply, so both come off one stream.
        let deadline = Instant::now() + Duration::from_secs(45);
        let (mut created, mut installed) = (None, None);
        while created.is_none() || installed.is_none() {
            match recv(&mut daemon, "the auto-create's reply and layout").await {
                Frame::Created { session, .. } => created = Some(session),
                Frame::LayoutChanged {
                    workspace_id,
                    layout,
                    rev,
                    ..
                } if workspace_id == "ws-self-taught" => installed = Some((layout, rev)),
                Frame::Error { message, .. } => panic!("the auto-create failed: {message}"),
                _ => {}
            }
            assert!(
                Instant::now() < deadline,
                "the requester was never told its own layout"
            );
        }
        let (layout, rev) = installed.expect("the layout event");
        assert_eq!(rev, 1);
        assert_eq!(
            layout.terminal_sessions(),
            vec![created.expect("the session").id]
        );
    });
}
