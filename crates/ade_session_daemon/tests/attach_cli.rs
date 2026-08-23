//! Integration tests for `ade-daemon attach`, driving the **real** built binary
//! against a real daemon over a real socket.
//!
//! Its stdio is piped rather than a tty, which is exactly the case the client
//! has to handle by skipping raw mode — so these tests exercise the pump, not
//! the termios dance, and run headless on any machine.
#![cfg(unix)]

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use ade_session::client::Connection;
use ade_session::proto::{Frame, Hello, MAX_GENERATION, SessionId, SessionInfo};
use ade_session_daemon::{RunningServer, Server, ServerConfig};
use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
use smol::net::unix::UnixStream;
use smol::process::{Child, Command, Stdio};
use tempfile::TempDir;

/// How long any single step may take before a test gives up.
const TIMEOUT: Duration = Duration::from_secs(45);

fn server() -> (TempDir, RunningServer) {
    let dir = TempDir::new().expect("temp dir");
    let config = ServerConfig::new(dir.path().join("daemon.sock"), dir.path().join("state"));
    let running = Server::spawn(config).expect("spawning server");
    (dir, running)
}

/// The attach client, with piped (non-tty) stdio.
fn attach(socket: &Path, session_id: &str) -> Child {
    attach_at(session_id, "--socket", &socket.display().to_string())
}

fn attach_at(session_id: &str, flag: &str, address: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_ade-daemon"))
        .arg("attach")
        .arg(session_id)
        .arg(flag)
        .arg(address)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning the attach client")
}

/// A loopback port that forwards every connection to the daemon's socket, and
/// the task serving it — stands in for the local end of a TCP-mode `ssh -L`
/// forward without needing an ssh server, since what is under test here is the
/// client's `--tcp` path and not ssh's.
///
/// The task is returned so the caller keeps it alive; dropping it closes the
/// listener, exactly as dropping a `HostForward` would.
struct BridgeCut {
    disconnected: smol::channel::Sender<()>,
    retrying: smol::channel::Sender<()>,
    resume: smol::channel::Receiver<()>,
}

async fn tcp_bridge(socket: &Path) -> (u16, smol::channel::Sender<BridgeCut>, smol::Task<()>) {
    let listener = smol::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("binding the bridge");
    let port = listener.local_addr().expect("the bridge's port").port();
    let socket = socket.to_path_buf();
    let (disconnect, disconnects) = smol::channel::unbounded::<BridgeCut>();
    let task = smol::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let Ok(daemon) = UnixStream::connect(&socket).await else {
                break;
            };
            let (mut to_daemon, mut to_client) = (daemon.clone(), client.clone());
            let transfer = async {
                let up = smol::io::copy(client, &mut to_daemon);
                let down = smol::io::copy(daemon, &mut to_client);
                if let Err(error) = smol::future::or(up, down).await {
                    assert!(
                        matches!(
                            error.kind(),
                            std::io::ErrorKind::BrokenPipe
                                | std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::UnexpectedEof
                        ),
                        "TCP bridge stopped copying: {error}"
                    );
                }
                None
            };
            let cut = async { disconnects.recv().await.ok() };
            if let Some(cut) = smol::future::or(transfer, cut).await {
                drop(to_daemon);
                drop(to_client);
                if cut.disconnected.send(()).await.is_err() {
                    break;
                }
                loop {
                    let resumed = async { cut.resume.recv().await.ok().map(|()| true) };
                    let rejected = async {
                        listener.accept().await.ok().map(|(client, _)| {
                            drop(client);
                            false
                        })
                    };
                    match smol::future::or(resumed, rejected).await {
                        Some(true) => break,
                        Some(false) => {
                            if cut.retrying.send(()).await.is_err() {
                                return;
                            }
                        }
                        None => return,
                    }
                }
            }
        }
    });
    (port, disconnect, task)
}

/// A control connection: the app's side of the protocol, used here to set the
/// session up and to observe it from outside the client under test.
async fn control(socket: &Path) -> Connection<UnixStream> {
    let stream = UnixStream::connect(socket).await.expect("connecting");
    let mut connection = Connection::new(stream);
    let ack = connection
        .handshake(Hello::current())
        .await
        .expect("handshake");
    assert_eq!(ack.generation, MAX_GENERATION);
    connection
}

/// Run `future`, failing the test rather than hanging forever.
///
/// The timeout is a blocking sleep on smol's blocking pool rather than a
/// `smol::Timer`, which the workspace lints disallow.
async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    let work = async { Some(future.await) };
    let timeout = async {
        smol::unblock(|| std::thread::sleep(TIMEOUT)).await;
        None
    };
    match smol::future::or(work, timeout).await {
        Some(value) => value,
        None => panic!("timed out waiting for {what}"),
    }
}

/// A workspace to put sessions in: the daemon refuses a session in a record it
/// does not hold, and the attach CLI cannot make one.
async fn workspace(connection: &mut Connection<UnixStream>, root: &Path) -> String {
    connection
        .send(&Frame::CreateWorkspace {
            root: root.display().to_string(),
            name: Some("attached".into()),
            project_id: None,
            project_identity: None,
            request_id: Some(2),
            env: Vec::new(),
            cols: None,
            rows: None,
        })
        .await
        .expect("sending CreateWorkspace");
    within("Workspace", async {
        loop {
            if let Frame::Workspace {
                workspace,
                request_id: Some(2),
                ..
            } = connection.recv().await.expect("receiving a frame")
            {
                return workspace.id;
            }
        }
    })
    .await
}

async fn create(connection: &mut Connection<UnixStream>, cwd: &Path, command: &str) -> SessionInfo {
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
            instance_label: "attached".into(),
            scrollback_bytes: None,
            request_id: Some(1),
        })
        .await
        .expect("sending CreateSession");
    within("Created", async {
        loop {
            if let Frame::Created { session, .. } =
                connection.recv().await.expect("receiving a frame")
            {
                return session;
            }
        }
    })
    .await
}

async fn kill(connection: &mut Connection<UnixStream>, id: &SessionId) {
    connection
        .send(&Frame::Kill {
            session_id: id.clone(),
            request_id: Some(2),
        })
        .await
        .expect("sending Kill");
    within("Removed", async {
        loop {
            match connection.recv().await.expect("receiving a frame") {
                Frame::Removed { session_id } if &session_id == id => return,
                Frame::Error { message, .. } => panic!("kill failed: {message}"),
                _ => {}
            }
        }
    })
    .await;
}

/// Attach from the control connection and read until `needle` shows up, so a
/// test can watch a session from outside the client under test.
async fn watch_until(connection: &mut Connection<UnixStream>, id: &SessionId, needle: &[u8]) {
    connection
        .send(&Frame::Attach {
            session_id: id.clone(),
            view_id: None,
            request_id: Some(3),
        })
        .await
        .expect("sending Attach");
    within(
        &format!("{:?} from the session", String::from_utf8_lossy(needle)),
        async {
            let mut seen = Vec::new();
            while !contains(&seen, needle) {
                match connection.recv().await.expect("receiving a frame") {
                    Frame::Replay { bytes, .. } | Frame::Output { bytes, .. } => {
                        seen.extend_from_slice(&bytes)
                    }
                    _ => {}
                }
            }
        },
    )
    .await;
}

/// Read the child's stdout until `needle` has been seen, and return everything
/// read so far.
async fn read_until(stdout: &mut smol::process::ChildStdout, needle: &[u8]) -> Vec<u8> {
    within(
        &format!(
            "{:?} on the client's stdout",
            String::from_utf8_lossy(needle)
        ),
        async {
            let mut seen = Vec::new();
            let mut buffer = [0u8; 4096];
            while !contains(&seen, needle) {
                let read = stdout.read(&mut buffer).await.expect("reading stdout");
                assert!(read > 0, "the client closed stdout before {needle:?}");
                seen.extend_from_slice(&buffer[..read]);
            }
            seen
        },
    )
    .await
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The whole point of attaching: the session's output arrives first, as data,
/// and the client writes it out untouched.
///
/// The stream opens with the retained history, raw, and a *repaint* of the
/// session's screen follows to repair the visible rows at the current size.
/// So HELLO arrives twice: once as history, once painted at its position
/// inside the repaint's synchronized-output block.
#[test]
fn attaching_replays_the_sessions_output_first() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut control = control(server.socket_path()).await;
        let session = create(&mut control, dir.path(), "sh -c 'printf HELLO; sleep 30'").await;
        // Make sure the bytes have reached the session's screen before the
        // client attaches, so what it prints can only have come from the replay.
        watch_until(&mut control, &session.id, b"HELLO").await;

        let mut client = attach(server.socket_path(), session.id.as_str());
        let mut stdout = client.stdout.take().expect("piped stdout");
        let seen = read_until(&mut stdout, b"\x1b[1;1HHELLO").await;
        assert!(
            seen.starts_with(b"HELLO"),
            "the retained history leads the stream, got {seen:?}"
        );
        assert!(
            contains(&seen, b"\x1b[?2026h"),
            "the repaint follows the history, got {seen:?}"
        );

        kill(&mut control, &session.id).await;
        let status = within("the client to exit", client.status()).await;
        assert!(status.expect("client status").success());
    });
}

/// stdin → the session's pty, watched from a second connection.
#[test]
fn what_is_typed_at_the_client_reaches_the_session() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut control = control(server.socket_path()).await;
        let session = create(&mut control, dir.path(), "cat").await;

        let mut client = attach(server.socket_path(), session.id.as_str());
        let mut stdout = client.stdout.take().expect("piped stdout");
        let mut stdin = client.stdin.take().expect("piped stdin");

        // `cat` echoes, so the marker coming back out is proof it went in —
        // and reading it from the client's own stdout proves the round trip
        // took the attach stream, not a private channel.
        stdin.write_all(b"PING\n").await.expect("writing stdin");
        stdin.flush().await.expect("flushing stdin");
        read_until(&mut stdout, b"PING").await;
        // The session, seen from outside the client, has it too.
        watch_until(&mut control, &session.id, b"PING").await;

        kill(&mut control, &session.id).await;
        let status = within("the client to exit", client.status()).await;
        assert!(status.expect("client status").success());
    });
}

/// A session killed from elsewhere ends the client on its own — it does not sit
/// on a silent socket waiting for a pty that is gone.
#[test]
fn killing_the_session_elsewhere_ends_the_client() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut control = control(server.socket_path()).await;
        let session = create(&mut control, dir.path(), "sh -c 'printf READY; sleep 30'").await;

        let mut client = attach(server.socket_path(), session.id.as_str());
        let mut stdout = client.stdout.take().expect("piped stdout");
        // Only kill once the client is provably attached; otherwise the test
        // would be racing the attach and asserting the wrong failure.
        read_until(&mut stdout, b"READY").await;

        kill(&mut control, &session.id).await;
        let status = within("the client to exit", client.status()).await;
        assert!(
            status.expect("client status").success(),
            "a killed session is not the client's failure"
        );
    });
}

/// The session has to exist: attach never creates one, so an unknown id is an
/// error and not a quietly empty terminal.
///
/// **The attach CLI is attach-only**, which is the whole of its place in the
/// spec — a legitimate pipe peer that is not a zorca client. It sends `attach`,
/// `write` and `resize` and nothing else, so no path through it can create a
/// session or a workspace. Asserted here on the daemon it just talked to: the
/// failed attach left the table and the ledger exactly as empty as it found
/// them.
#[test]
fn attaching_to_an_unknown_session_fails_and_creates_nothing() {
    let (dir, server) = server();
    smol::block_on(async {
        let client = attach(server.socket_path(), "no-such-session");
        let output = within("the client to exit", client.output())
            .await
            .expect("client output");
        assert!(!output.status.success(), "an unknown session is an error");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("no-such-session"),
            "the error names the session: {stderr}"
        );

        let mut control = control(server.socket_path()).await;
        control
            .send(&Frame::ListSessions {
                request_id: Some(80),
            })
            .await
            .expect("sending ListSessions");
        match within("the session list", control.recv())
            .await
            .expect("reply")
        {
            Frame::SessionList { sessions, .. } => {
                assert!(sessions.is_empty(), "the attach created {sessions:?}")
            }
            other => panic!("expected SessionList, got {other:?}"),
        }
        control
            .send(&Frame::ListWorkspaces {
                request_id: Some(81),
            })
            .await
            .expect("sending ListWorkspaces");
        match within("the workspace list", control.recv())
            .await
            .expect("reply")
        {
            Frame::WorkspaceList { workspaces, .. } => {
                assert!(workspaces.is_empty(), "the attach created {workspaces:?}")
            }
            other => panic!("expected WorkspaceList, got {other:?}"),
        }
        assert!(
            !dir.path().join("state").join("sessions.json").exists(),
            "an attach-only client made the daemon write a ledger"
        );
    });
}

/// `--tcp` is the same client over a different byte pipe: it replays, it
/// carries what is typed, and its death is still a detach. This is the path a
/// Windows client takes, since its ssh forwards to a loopback port rather than
/// to a Unix socket.
#[test]
fn attaching_over_tcp_reaches_the_same_session() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut control = control(server.socket_path()).await;
        let session = create(&mut control, dir.path(), "cat").await;
        let (port, _disconnect, _bridge) = tcp_bridge(server.socket_path()).await;

        let mut client = attach_at(session.id.as_str(), "--tcp", &format!("127.0.0.1:{port}"));
        let mut stdout = client.stdout.take().expect("piped stdout");
        let mut stdin = client.stdin.take().expect("piped stdin");

        stdin.write_all(b"PING\n").await.expect("writing stdin");
        stdin.flush().await.expect("flushing stdin");
        read_until(&mut stdout, b"PING").await;
        watch_until(&mut control, &session.id, b"PING").await;

        kill(&mut control, &session.id).await;
        let status = within("the client to exit", client.status()).await;
        assert!(status.expect("client status").success());
    });
}

/// A forwarded channel can disappear while the daemon and its pty stay alive.
/// The attach client must reconnect to the same session instead of turning that
/// transport failure into the end of the terminal.
#[test]
fn attaching_over_tcp_reconnects_after_the_forward_drops() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut control = control(server.socket_path()).await;
        let session = create(&mut control, dir.path(), "cat").await;
        let (port, disconnect, _bridge) = tcp_bridge(server.socket_path()).await;

        let mut client = attach_at(session.id.as_str(), "--tcp", &format!("127.0.0.1:{port}"));
        let mut stdout = client.stdout.take().expect("piped stdout");
        let mut stdin = client.stdin.take().expect("piped stdin");

        stdin.write_all(b"BEFORE\n").await.expect("writing stdin");
        stdin.flush().await.expect("flushing stdin");
        read_until(&mut stdout, b"BEFORE").await;

        let (disconnected, observed) = smol::channel::bounded(1);
        let (retrying, retried) = smol::channel::unbounded();
        let (resume, resumed) = smol::channel::bounded(1);
        disconnect
            .send(BridgeCut {
                disconnected,
                retrying,
                resume: resumed,
            })
            .await
            .expect("asking the bridge to disconnect");
        within("the forwarded connection to close", observed.recv())
            .await
            .expect("observing the bridge disconnect");
        within("the client to retry the forward", retried.recv())
            .await
            .expect("observing a rejected retry");

        control
            .send(&Frame::Write {
                session_id: session.id.clone(),
                bytes: b"WHILE_AWAY\n".to_vec(),
            })
            .await
            .expect("writing while the forward is unavailable");
        watch_until(&mut control, &session.id, b"WHILE_AWAY").await;
        stdin
            .write_all(b"QUEUED\n")
            .await
            .expect("writing stdin while disconnected");
        stdin.flush().await.expect("flushing queued stdin");
        resume.send(()).await.expect("resuming the bridge");
        let replayed = read_until(&mut stdout, b"WHILE_AWAY").await;
        assert!(
            contains(&replayed, b"\x1bc"),
            "the dirty terminal was not reset before replay: {replayed:?}"
        );
        read_until(&mut stdout, b"QUEUED").await;

        stdin.write_all(b"AFTER\n").await.expect("writing stdin");
        stdin.flush().await.expect("flushing stdin");
        read_until(&mut stdout, b"AFTER").await;

        kill(&mut control, &session.id).await;
        let status = within("the client to exit", client.status()).await;
        assert!(status.expect("client status").success());
    });
}

/// If the session is removed while its forward is down, reconnecting must end
/// the existing client instead of retrying a permanent Attach rejection.
#[test]
fn killing_a_session_while_its_forward_is_down_ends_the_client() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut control = control(server.socket_path()).await;
        let session = create(&mut control, dir.path(), "cat").await;
        let (port, disconnect, _bridge) = tcp_bridge(server.socket_path()).await;

        let mut client = attach_at(session.id.as_str(), "--tcp", &format!("127.0.0.1:{port}"));
        let mut stdout = client.stdout.take().expect("piped stdout");
        let mut stderr = client.stderr.take().expect("piped stderr");
        let mut stdin = client.stdin.take().expect("piped stdin");
        stdin.write_all(b"READY\n").await.expect("writing stdin");
        stdin.flush().await.expect("flushing stdin");
        read_until(&mut stdout, b"READY").await;

        let (disconnected, observed) = smol::channel::bounded(1);
        let (retrying, retried) = smol::channel::unbounded();
        let (resume, resumed) = smol::channel::bounded(1);
        disconnect
            .send(BridgeCut {
                disconnected,
                retrying,
                resume: resumed,
            })
            .await
            .expect("asking the bridge to disconnect");
        within("the forwarded connection to close", observed.recv())
            .await
            .expect("observing the bridge disconnect");
        within("the client to retry the forward", retried.recv())
            .await
            .expect("observing a rejected retry");

        kill(&mut control, &session.id).await;
        resume.send(()).await.expect("resuming the bridge");
        let status = within("the client to exit", client.status())
            .await
            .expect("client status");
        assert!(status.success(), "a killed session is not a client failure");
        let mut error = String::new();
        stderr
            .read_to_string(&mut error)
            .await
            .expect("reading stderr");
        assert!(error.contains("[ade: session was killed]"), "{error}");
    });
}

/// Nothing listening on the port is the same error as nothing listening on the
/// socket — a forward whose far end is gone must not read as an empty terminal.
#[test]
fn attaching_over_tcp_to_nothing_fails_with_the_address() {
    smol::block_on(async {
        // Reserved and released, so it is free and almost certainly unbound.
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("reserving a port");
        let port = listener.local_addr().expect("the port").port();
        drop(listener);

        let client = attach_at("any-session", "--tcp", &format!("127.0.0.1:{port}"));
        let output = within("the client to exit", client.output())
            .await
            .expect("client output");
        assert!(!output.status.success(), "no daemon is an error");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!(
                "no ADE session daemon is listening on 127.0.0.1:{port}"
            )),
            "the error names the address: {stderr}"
        );
    });
}

/// Two addresses is a caller that does not know which transport it is on, and
/// picking one for it would attach to whichever daemon happened to answer.
#[test]
fn attach_refuses_both_a_socket_and_a_tcp_address() {
    let (_dir, server) = server();
    smol::block_on(async {
        let client = Command::new(env!("CARGO_BIN_EXE_ade-daemon"))
            .arg("attach")
            .arg("any-session")
            .arg("--socket")
            .arg(server.socket_path())
            .arg("--tcp")
            .arg("127.0.0.1:1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawning the attach client");
        let output = within("the client to exit", client.output())
            .await
            .expect("client output");
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--socket or --tcp, not both"),
            "the error says what is wrong: {stderr}"
        );
    });
}

/// No daemon means nothing to attach to. It must **not** start one: an attach
/// has a session to reach, and an empty daemon has none.
#[test]
fn attaching_without_a_daemon_fails_instead_of_starting_one() {
    let dir = TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    smol::block_on(async {
        let client = attach(&socket, "any-session");
        let output = within("the client to exit", client.output())
            .await
            .expect("client output");
        assert!(!output.status.success(), "no daemon is an error");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("no ADE session daemon is listening"),
            "the error says what is missing: {stderr}"
        );
        // Nothing was started, and nothing was left behind.
        assert!(!socket.exists(), "no daemon was started");
    });
}

/// The daemon's own instance id, which is what `--expected-daemon-id` names.
async fn daemon_id(socket: &Path) -> String {
    let stream = UnixStream::connect(socket).await.expect("connecting");
    let mut connection = Connection::new(stream);
    connection
        .handshake(Hello::current())
        .await
        .expect("handshake")
        .instance_id
        .expect("the daemon reports an instance id")
}

fn fenced_attach(socket: &Path, session_id: &str, expected: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_ade-daemon"))
        .arg("attach")
        .arg(session_id)
        .arg("--socket")
        .arg(socket)
        .arg("--expected-daemon-id")
        .arg(expected)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning the attach client")
}

/// An address outlives the daemon behind it — a socket path is rebound, a
/// forwarded port is reused — so a terminal that names its daemon reaches that
/// daemon and no other. Both halves against one real daemon: the id it reports
/// attaches, any other id is refused before a frame goes out.
#[test]
fn attach_is_fenced_to_the_daemon_it_names() {
    let (dir, server) = server();
    smol::block_on(async {
        let mut control = control(server.socket_path()).await;
        let session = create(&mut control, dir.path(), "sh -c 'printf HELLO; sleep 30'").await;
        watch_until(&mut control, &session.id, b"HELLO").await;
        let id = daemon_id(server.socket_path()).await;

        let mut client = fenced_attach(server.socket_path(), session.id.as_str(), &id);
        let mut stdout = client.stdout.take().expect("piped stdout");
        let seen = read_until(&mut stdout, b"HELLO").await;
        assert!(
            contains(&seen, b"HELLO"),
            "the fenced attach did not replay the session: {seen:?}"
        );
        kill(&mut control, &session.id).await;
        let status = within("the client to exit", client.status()).await;
        assert!(status.expect("client status").success());

        let client = fenced_attach(
            server.socket_path(),
            session.id.as_str(),
            "some-other-daemon",
        );
        let output = within("the client to exit", client.output())
            .await
            .expect("client output");
        assert!(!output.status.success(), "another daemon is an error");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("some-other-daemon") && stderr.contains(&id),
            "the error names the daemon expected and the one that answered: {stderr}"
        );
    });
}

/// Attach-only, refused by name: a daemon does not check its own identity, and
/// a proxy carries whatever the client behind it handshakes with.
#[test]
fn expected_daemon_id_is_only_for_attach() {
    let dir = TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock");
    smol::block_on(async {
        let daemon = Command::new(env!("CARGO_BIN_EXE_ade-daemon"))
            .arg("--ensure")
            .arg("--socket")
            .arg(&socket)
            .arg("--expected-daemon-id")
            .arg("daemon-a")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawning ade-daemon");
        let output = within("ade-daemon to exit", daemon.output())
            .await
            .expect("its output");
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--expected-daemon-id is only for attach"),
            "the error says where the flag belongs: {stderr}"
        );
        // Refused before anything is started, the same as `--tcp` is.
        assert!(!socket.exists(), "no daemon was started");
    });
}
