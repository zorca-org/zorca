//! Protocol tests over an in-process duplex stream: no daemon, no network.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use ade_session::client::Connection;
use ade_session::framing::{
    MAX_FRAME_BYTES, MAX_OP_BYTES, ReadFrameError, decode_frame, encode_frame, read_frame,
    rejection_frame, write_frame,
};
use ade_session::proto::{
    Frame, Hello, HelloAck, KNOWN_OPS, LAYOUT_SCHEMA_VERSION, LayoutDoc, LayoutNode,
    MAX_GENERATION, MIN_GENERATION, SessionId, SessionInfo, SessionStatus, SplitDir, Tab,
    WorkspaceInfo, effective_capabilities, error_code, select_generation, validate_capabilities,
};
use futures::executor::block_on;
use futures::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use serde_json::{Value, json};

const PIPE_CAPACITY: usize = 64 * 1024;

/// One half of an in-memory duplex, built from a pair of `piper` pipes.
struct PipeStream {
    reader: piper::Reader,
    writer: piper::Writer,
}

/// `(client, server)` — each writes into the other's read half.
fn duplex() -> (PipeStream, PipeStream) {
    let (client_reader, server_writer) = piper::pipe(PIPE_CAPACITY);
    let (server_reader, client_writer) = piper::pipe(PIPE_CAPACITY);
    (
        PipeStream {
            reader: client_reader,
            writer: client_writer,
        },
        PipeStream {
            reader: server_reader,
            writer: server_writer,
        },
    )
}

impl AsyncRead for PipeStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().reader).poll_read(cx, buf)
    }
}

impl AsyncWrite for PipeStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().writer).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_close(cx)
    }
}

/// A raw JSON payload wrapped in the 4-byte length prefix, for the tests that
/// hand-build a frame instead of encoding one.
fn framed(json: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(4 + json.len());
    framed.extend_from_slice(&(json.len() as u32).to_be_bytes());
    framed.extend_from_slice(json);
    framed
}

/// What a gen-2 daemon answers with.
fn hello_ack() -> HelloAck {
    HelloAck {
        daemon_version: "0.1.0".into(),
        protocol_version: MAX_GENERATION,
        host_os: "linux".into(),
        min_generation: MIN_GENERATION,
        max_generation: MAX_GENERATION,
        generation: MAX_GENERATION,
        capabilities: vec!["ade.persist_before_ack".into()],
        degraded: false,
        binary_hash: Some("a".repeat(64)),
        upgrade_ready: Some(true),
        instance_id: Some("6f1a9f60-0e5f-4d2e-9a1a-8f2b3c4d5e6f".into()),
        request_id: Some(1),
    }
}

fn session_info(id: &str) -> SessionInfo {
    SessionInfo {
        id: SessionId::new(id),
        workspace_id: "ws-1".into(),
        agent_kind: "claude".into(),
        instance_label: "main".into(),
        cwd: "/home/u/proj".into(),
        created_at: 1_754_200_000,
        status: SessionStatus::Working,
    }
}

/// A split holding a terminal-and-editor leaf beside a two-terminal leaf.
fn nested_layout() -> LayoutDoc {
    LayoutDoc::new(LayoutNode::Split {
        dir: SplitDir::Horizontal,
        ratio: 0.4,
        children: Box::new([
            LayoutNode::leaf(vec![
                Tab::Terminal {
                    session_id: SessionId::new("s-1"),
                },
                Tab::Editor {
                    path: "/home/u/proj/src/main.rs".into(),
                },
            ]),
            LayoutNode::Leaf {
                tabs: vec![
                    Tab::Terminal {
                        session_id: SessionId::new("s-2"),
                    },
                    Tab::Terminal {
                        session_id: SessionId::new("s-3"),
                    },
                ],
                active: 1,
                focused: true,
            },
        ]),
    })
}

fn workspace_info() -> WorkspaceInfo {
    WorkspaceInfo {
        id: "w-1".into(),
        name: "proj".into(),
        project_id: Some("proj".into()),
        project_identity: Some("/home/u/proj".into()),
        project_root: "/home/u/proj".into(),
        project_scope_rev: 3,
        created_at: 1_754_200_000,
        layout_rev: 4,
        layout: nested_layout(),
    }
}

/// Every variant of `Frame`, so the round-trip test covers the whole surface.
fn every_variant() -> Vec<Frame> {
    vec![
        Frame::Hello(Hello::current()),
        Frame::CreateSession {
            workspace_id: "ws-1".into(),
            cwd: "/home/u/proj".into(),
            project_id: Some("proj".into()),
            project_identity: Some("/home/u/proj".into()),
            command: "claude".into(),
            env: vec![("TERM".into(), "xterm-256color".into())],
            cols: 120,
            rows: 40,
            agent_kind: "claude".into(),
            instance_label: "main".into(),
            scrollback_bytes: Some(1 << 20),
            request_id: Some(7),
        },
        Frame::CreateSession {
            workspace_id: "ws-2".into(),
            cwd: "/srv/app".into(),
            project_id: None,
            project_identity: None,
            command: "aider".into(),
            env: Vec::new(),
            cols: 80,
            rows: 24,
            agent_kind: "aider".into(),
            instance_label: "two".into(),
            scrollback_bytes: None,
            request_id: None,
        },
        Frame::ListSessions {
            request_id: Some(11),
        },
        Frame::Attach {
            session_id: SessionId::new("s-1"),
            view_id: None,
            request_id: Some(12),
        },
        Frame::Attach {
            session_id: SessionId::new("s-1"),
            view_id: Some("view-1".into()),
            request_id: Some(12),
        },
        Frame::Detach {
            session_id: SessionId::new("s-1"),
            request_id: None,
        },
        Frame::Write {
            session_id: SessionId::new("s-1"),
            bytes: b"ls -la\n".to_vec(),
        },
        Frame::Resize {
            session_id: SessionId::new("s-1"),
            cols: 200,
            rows: 60,
        },
        Frame::FocusSession {
            session_id: SessionId::new("s-1"),
            view_id: "view-1".into(),
            hover: false,
        },
        Frame::Kill {
            session_id: SessionId::new("s-1"),
            request_id: Some(13),
        },
        Frame::Subscribe {
            request_id: Some(14),
        },
        Frame::Shutdown {
            force: false,
            request_id: Some(21),
        },
        Frame::Shutdown {
            force: true,
            request_id: None,
        },
        Frame::ShutdownAck {
            request_id: Some(21),
        },
        Frame::HelloAck(hello_ack()),
        Frame::SessionList {
            sessions: vec![session_info("s-1"), session_info("s-2")],
            request_id: Some(11),
        },
        Frame::SessionList {
            sessions: Vec::new(),
            request_id: None,
        },
        Frame::Created {
            session: session_info("s-3"),
            persisted: true,
            request_id: Some(7),
        },
        Frame::Removed {
            session_id: SessionId::new("s-3"),
        },
        Frame::Replay {
            session_id: SessionId::new("s-1"),
            bytes: b"\x1b[2Jscrollback".to_vec(),
            truncated: true,
        },
        Frame::Output {
            session_id: SessionId::new("s-1"),
            bytes: vec![0, 1, 2, 255],
        },
        Frame::Status {
            session_id: SessionId::new("s-1"),
            status: SessionStatus::NeedsInput,
            since: 1_754_200_100,
        },
        Frame::Status {
            session_id: SessionId::new("s-2"),
            status: SessionStatus::Idle,
            since: 1_754_200_200,
        },
        Frame::Exited {
            session_id: SessionId::new("s-2"),
            exit_code: Some(130),
        },
        Frame::Exited {
            session_id: SessionId::new("s-2"),
            exit_code: None,
        },
        Frame::Error {
            session_id: Some(SessionId::new("s-9")),
            workspace_id: None,
            code: error_code::NOT_FOUND.into(),
            message: "no such session".into(),
            request_id: Some(12),
        },
        // The unsolicited half: no `rid` to answer, but a workspace to name.
        Frame::Error {
            session_id: None,
            workspace_id: Some("w-1".into()),
            code: error_code::PERSIST_FAILED.into(),
            message: "the layout applied but could not be recorded".into(),
            request_id: None,
        },
        Frame::CreateWorkspace {
            root: "/home/u/proj".into(),
            name: Some("proj".into()),
            project_id: Some("proj".into()),
            project_identity: Some("/home/u/proj".into()),
            request_id: Some(15),
            env: Vec::new(),
            cols: None,
            rows: None,
        },
        Frame::OpenWorkspace {
            workspace_id: "w-1".into(),
            request_id: Some(16),
        },
        Frame::ListWorkspaces {
            request_id: Some(17),
        },
        Frame::UpdateLayout {
            workspace_id: "w-1".into(),
            layout: nested_layout(),
            rev: 5,
            request_id: Some(18),
        },
        Frame::RenameWorkspace {
            workspace_id: "w-1".into(),
            name: "vector db spike".into(),
            request_id: Some(19),
        },
        Frame::UpdateWorkspaceProject {
            workspace_id: "w-1".into(),
            project_id: "proj".into(),
            project_identity: "/home/u/proj".into(),
            project_root: Some("/home/u/worktrees/proj/feature".into()),
            minimum_scope_rev: Some(8),
            request_id: Some(20),
        },
        Frame::KillWorkspace {
            workspace_id: "w-1".into(),
            request_id: Some(21),
        },
        Frame::Workspace {
            workspace: workspace_info(),
            sessions: vec![session_info("s-1"), session_info("s-2")],
            persisted: true,
            request_id: Some(16),
        },
        Frame::WorkspaceList {
            workspaces: vec![workspace_info()],
            request_id: Some(17),
        },
        Frame::LayoutChanged {
            workspace_id: "w-1".into(),
            layout: nested_layout(),
            rev: 5,
            persisted: true,
            request_id: None,
        },
        Frame::WorkspaceRemoved {
            workspace_id: "w-1".into(),
            persisted: true,
            request_id: Some(20),
        },
    ]
}

/// §8.5's `persisted` flag, against §4's rules for a field added after its op
/// shipped: absent means `true`, so a daemon that persists normally puts
/// nothing new on the wire and an older peer's frames keep their old meaning.
/// Only a degraded daemon's ack says anything at all.
#[test]
fn the_persisted_flag_is_absent_unless_it_is_false() {
    let normal = encode_frame(&Frame::WorkspaceRemoved {
        workspace_id: "w-1".into(),
        persisted: true,
        request_id: Some(3),
    })
    .unwrap();
    let value: Value = serde_json::from_slice(&normal).unwrap();
    assert!(
        !value["body"].as_object().unwrap().contains_key("persisted"),
        "a normal ack must be byte-identical to one from before the field: {}",
        String::from_utf8_lossy(&normal)
    );

    let degraded = encode_frame(&Frame::WorkspaceRemoved {
        workspace_id: "w-1".into(),
        persisted: false,
        request_id: Some(3),
    })
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&degraded).unwrap()["body"]["persisted"],
        json!(false)
    );
    assert_eq!(
        decode_frame(&degraded).unwrap(),
        Frame::WorkspaceRemoved {
            workspace_id: "w-1".into(),
            persisted: false,
            request_id: Some(3),
        }
    );

    // The reader half: an older daemon's ack has no such field, and it never
    // meant "not recorded".
    assert_eq!(
        decode_frame(br#"{"op":"created","rid":4,"body":{"session":{"id":"s-1","workspace_id":"ws-1","agent_kind":"claude","instance_label":"main","cwd":"/home/u/proj","created_at":1754200000,"status":"working"}}}"#)
            .unwrap(),
        Frame::Created {
            session: session_info("s-1"),
            persisted: true,
            request_id: Some(4),
        }
    );
}

#[test]
fn updating_a_workspace_project_keeps_the_root_backward_compatible() {
    let legacy = Frame::UpdateWorkspaceProject {
        workspace_id: "w-1".into(),
        project_id: "proj".into(),
        project_identity: "/home/u/proj".into(),
        project_root: None,
        minimum_scope_rev: None,
        request_id: Some(20),
    };
    assert_eq!(
        decode_frame(
            br#"{"op":"update_workspace_project","rid":20,"body":{"workspace_id":"w-1","project_id":"proj","project_identity":"/home/u/proj"}}"#,
        )
        .unwrap(),
        legacy
    );
    assert!(
        !serde_json::from_slice::<Value>(&encode_frame(&legacy).unwrap()).unwrap()["body"]
            .as_object()
            .unwrap()
            .contains_key("project_root")
    );
    assert!(
        !serde_json::from_slice::<Value>(&encode_frame(&legacy).unwrap()).unwrap()["body"]
            .as_object()
            .unwrap()
            .contains_key("minimum_scope_rev")
    );

    let encoded = encode_frame(&Frame::UpdateWorkspaceProject {
        workspace_id: "w-1".into(),
        project_id: "proj".into(),
        project_identity: "/home/u/proj".into(),
        project_root: Some(" /home/u/worktrees/proj/feature ".into()),
        minimum_scope_rev: Some(8),
        request_id: Some(20),
    })
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&encoded).unwrap()["body"]["project_root"],
        json!(" /home/u/worktrees/proj/feature ")
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&encoded).unwrap()["body"]["minimum_scope_rev"],
        json!(8)
    );
}

#[test]
fn a_legacy_workspace_scope_starts_at_revision_zero() {
    let mut encoded = serde_json::from_slice::<Value>(
        &encode_frame(&Frame::Workspace {
            workspace: workspace_info(),
            sessions: Vec::new(),
            persisted: true,
            request_id: Some(20),
        })
        .unwrap(),
    )
    .unwrap();
    encoded["body"]["workspace"]
        .as_object_mut()
        .unwrap()
        .remove("project_scope_rev");

    match decode_frame(&serde_json::to_vec(&encoded).unwrap()).unwrap() {
        Frame::Workspace { workspace, .. } => assert_eq!(workspace.project_scope_rev, 0),
        other => panic!("expected Workspace, got {other:?}"),
    }
}

/// A generation-3 `create_workspace` carries the record's identity and nothing
/// else: the legacy first-session fields exist only to decode an old peer's
/// request, so a sender at 3 must put none of them on the wire.
#[test]
fn create_workspace_carries_nothing_but_the_record_at_generation_three() {
    let encoded = encode_frame(&Frame::CreateWorkspace {
        root: "/home/u/proj".into(),
        name: None,
        project_id: None,
        project_identity: None,
        request_id: Some(5),
        env: Vec::new(),
        cols: None,
        rows: None,
    })
    .unwrap();
    let value: Value = serde_json::from_slice(&encoded).unwrap();
    let body: Vec<&str> = value["body"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        body,
        ["root"],
        "encoded {}",
        String::from_utf8_lossy(&encoded)
    );
}

#[test]
fn frame_round_trip_covers_every_variant() {
    let frames = every_variant();
    // Guard against a variant being added to the enum but not to the fixture.
    assert_eq!(
        frames.len(),
        39,
        "add the new Frame variant to every_variant()"
    );

    let (mut client, mut server) = duplex();
    block_on(async {
        for frame in &frames {
            write_frame(&mut client, frame).await.unwrap();
            let decoded = read_frame(&mut server).await.unwrap();
            assert_eq!(&decoded, frame, "round trip changed the frame");
        }
    });
}

/// The honesty mechanism behind `KNOWN_OPS`. It is the list that decides
/// `unknown_op` from `malformed_body`, and it is hand-written next to an enum —
/// so it is checked against what the codec actually puts on the wire, in both
/// directions, rather than trusted.
#[test]
fn every_op_on_the_wire_is_listed_in_known_ops() {
    let mut listed: Vec<&str> = KNOWN_OPS.to_vec();
    listed.sort_unstable();
    let deduped = {
        let mut deduped = listed.clone();
        deduped.dedup();
        deduped
    };
    assert_eq!(listed, deduped, "KNOWN_OPS lists an op twice");

    let mut encoded: Vec<String> = every_variant()
        .iter()
        .map(|frame| {
            let payload = encode_frame(frame).unwrap();
            let value: Value = serde_json::from_slice(&payload).unwrap();
            value["op"].as_str().unwrap().to_owned()
        })
        .collect();
    encoded.sort();
    encoded.dedup();

    assert_eq!(
        encoded, listed,
        "KNOWN_OPS and the ops the codec emits have drifted apart"
    );
}

/// The cross-implementation fixture: the exact `hello` envelope, decoded and
/// encoded. pydaemon replicates this byte shape, so a change here is a change
/// to the protocol and not to a test. The decoded fixture is a gen-2 peer's
/// hello, which the *codec* must still read — §3.1's refusal is the handshake's
/// job, and it can only refuse what it could parse.
///
/// Key *order* is deliberately not asserted — JSON objects are unordered and
/// the Python twin's encoder will not match Rust's field order — but the key
/// *set* is: no `type`, no inline `request_id`, `rid` omitted when absent.
#[test]
fn the_hello_envelope_is_pinned_on_the_wire() {
    let wire = br#"{"op":"hello","rid":1,"body":{"min_generation":2,"max_generation":2,"capabilities":["ade.persist_before_ack"]}}"#;
    assert_eq!(
        decode_frame(wire).unwrap(),
        Frame::Hello(Hello {
            min_generation: 2,
            max_generation: 2,
            capabilities: vec!["ade.persist_before_ack".into()],
            request_id: Some(1),
        })
    );

    let payload = encode_frame(&Frame::Hello(Hello::current())).unwrap();
    let value: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(
        value,
        json!({
            "op": "hello",
            "body": {
                "min_generation": MIN_GENERATION,
                "max_generation": MAX_GENERATION,
                "capabilities": [],
            },
        }),
        "encoded {}",
        String::from_utf8_lossy(&payload)
    );

    let keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["op", "body"], "rid must be omitted when absent");
    let body = value["body"].as_object().unwrap();
    assert!(!body.contains_key("type"), "the tag must become `op`");
    assert!(
        !body.contains_key("request_id"),
        "the correlation id must become `rid`, never stay inline"
    );
}

#[test]
fn handshake_returns_the_daemons_ack() {
    let (client_stream, mut server) = duplex();
    let mut connection = Connection::new(client_stream);

    let expected = hello_ack();

    let ack = block_on(async {
        let fake_daemon = async {
            match read_frame(&mut server).await.unwrap() {
                Frame::Hello(hello) => {
                    assert_eq!(hello.min_generation, MIN_GENERATION);
                    assert_eq!(hello.max_generation, MAX_GENERATION);
                }
                other => panic!("expected Hello, got {other:?}"),
            }
            write_frame(&mut server, &Frame::HelloAck(expected.clone()))
                .await
                .unwrap();
        };
        let client = connection.handshake(Hello {
            request_id: Some(1),
            ..Hello::current()
        });
        let (ack, ()) = futures::join!(client, fake_daemon);
        ack.unwrap()
    });

    assert_eq!(ack, expected);
}

/// The daemon selects the generation and the client verifies it (§3.1). A
/// selection outside the client's range is a failed negotiation, and the error
/// has to name both ranges — it is read by whoever has to decide which end to
/// upgrade.
#[test]
fn handshake_rejects_a_generation_outside_the_clients_range() {
    let (client_stream, mut server) = duplex();
    let mut connection = Connection::new(client_stream);

    let result = block_on(async {
        let fake_daemon = async {
            read_frame(&mut server).await.unwrap();
            let ack = HelloAck {
                min_generation: MAX_GENERATION + 1,
                max_generation: MAX_GENERATION + 2,
                generation: MAX_GENERATION + 1,
                ..hello_ack()
            };
            write_frame(&mut server, &Frame::HelloAck(ack))
                .await
                .unwrap();
        };
        let (result, ()) = futures::join!(connection.handshake(Hello::current()), fake_daemon);
        result
    });

    let error = result.unwrap_err().to_string();
    assert!(
        error.contains(&format!("generation {}", MAX_GENERATION + 1)),
        "{error}"
    );
    assert!(
        error.contains(&format!("{MIN_GENERATION}..={MAX_GENERATION}")),
        "the client's own range must be in the message: {error}"
    );
    assert!(
        error.contains(&format!("{}..={}", MAX_GENERATION + 1, MAX_GENERATION + 2)),
        "the daemon's range must be in the message: {error}"
    );
}

/// An ack from a daemon deployed before binary identity existed: no
/// `binary_hash`, no `upgrade_ready`, and nothing said about `degraded` or
/// `capabilities`. It must decode — and decode to the absent meaning documented
/// on each field, which every client reads as "never upgrade it in place".
#[test]
fn an_ack_without_the_optional_fields_still_decodes() {
    let wire = br#"{"op":"hello_ack","rid":1,"body":{
        "daemon_version":"0.1.0","protocol_version":2,"host_os":"linux",
        "min_generation":2,"max_generation":2,"generation":2
    }}"#;
    match decode_frame(wire).expect("decoding a minimal ack") {
        Frame::HelloAck(ack) => {
            assert_eq!(ack.daemon_version, "0.1.0");
            assert_eq!(ack.binary_hash, None);
            assert_eq!(ack.upgrade_ready, None);
            // A daemon too old to name itself: the client identifies its host
            // by the spelling it was given, as it always did.
            assert_eq!(ack.instance_id, None);
            assert!(ack.capabilities.is_empty());
            assert!(!ack.degraded);
            assert_eq!(ack.request_id, Some(1));
        }
        other => panic!("expected HelloAck, got {other:?}"),
    }
}

#[test]
fn handshake_surfaces_an_error_frame() {
    let (client_stream, mut server) = duplex();
    let mut connection = Connection::new(client_stream);

    let result = block_on(async {
        let fake_daemon = async {
            read_frame(&mut server).await.unwrap();
            write_frame(
                &mut server,
                &Frame::Error {
                    session_id: None,
                    workspace_id: None,
                    code: error_code::UNSUPPORTED_GENERATION.into(),
                    message: "this daemon serves generations 3..=4".into(),
                    request_id: None,
                },
            )
            .await
            .unwrap();
        };
        let (result, ()) = futures::join!(connection.handshake(Hello::current()), fake_daemon);
        result
    });

    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("this daemon serves generations 3..=4"),
        "{error}"
    );
    assert!(
        error.contains(error_code::UNSUPPORTED_GENERATION),
        "the code is what a caller branches on: {error}"
    );
}

/// The handshake's three refusals all quote the daemon back, and a daemon can
/// be hostile or merely broken. These never reach the wire — they end up in a
/// log line or a banner — but the peer still chose every byte, so a megabyte of
/// `message` must not become a megabyte of `anyhow` error.
#[test]
fn a_handshake_refusal_does_not_quote_the_whole_daemon_back() {
    let refusals = [
        Frame::Error {
            session_id: None,
            workspace_id: None,
            code: error_code::INTERNAL.into(),
            message: "x".repeat(1024 * 1024),
            request_id: None,
        },
        // A generation outside this client's range (§3.1): the ack is
        // well-formed and the refusal names the daemon that sent it, so
        // `daemon_version` is peer-chosen text on the same footing.
        Frame::HelloAck(HelloAck {
            daemon_version: "z".repeat(1024 * 1024),
            generation: MAX_GENERATION + 1,
            min_generation: MAX_GENERATION + 1,
            max_generation: MAX_GENERATION + 1,
            ..hello_ack()
        }),
        // Not a HelloAck at all: the catch-all Debug-formats the frame, and a
        // frame is bounded only by MAX_FRAME_BYTES.
        Frame::Removed {
            session_id: SessionId("y".repeat(1024 * 1024)),
        },
    ];

    for refusal in refusals {
        let (client_stream, mut server) = duplex();
        let mut connection = Connection::new(client_stream);
        let error = block_on(async {
            let fake_daemon = async {
                read_frame(&mut server).await.unwrap();
                write_frame(&mut server, &refusal).await.unwrap();
            };
            let (result, ()) = futures::join!(connection.handshake(Hello::current()), fake_daemon);
            result
        })
        .unwrap_err()
        .to_string();

        assert!(
            error.len() < 1024,
            "a refusal grew with what the daemon sent: {} bytes",
            error.len()
        );
        assert!(error.contains('…'), "expected an elision: {error}");
    }
}

#[test]
fn frames_for_two_sessions_interleave_over_one_stream() {
    let sent: Vec<Frame> = (0..8)
        .map(|i| {
            let session_id = SessionId::new(if i % 2 == 0 { "s-a" } else { "s-b" });
            Frame::Output {
                session_id,
                bytes: format!("chunk {i}").into_bytes(),
            }
        })
        .collect();

    let (mut client, mut server) = duplex();
    let received = block_on(async {
        let writer = async {
            for frame in &sent {
                write_frame(&mut client, frame).await.unwrap();
            }
            client.close().await.unwrap();
        };
        let reader = async {
            let mut received = Vec::new();
            for _ in 0..sent.len() {
                received.push(read_frame(&mut server).await.unwrap());
            }
            received
        };
        let ((), received) = futures::join!(writer, reader);
        received
    });

    assert_eq!(received, sent, "interleaved frames must decode in order");
    let ids: Vec<&str> = received
        .iter()
        .map(|frame| frame.session_id().unwrap().as_str())
        .collect();
    assert_eq!(
        ids,
        ["s-a", "s-b", "s-a", "s-b", "s-a", "s-b", "s-a", "s-b"]
    );
}

#[test]
fn oversize_frames_are_rejected_without_killing_the_stream() {
    // Read side: a length prefix past the cap must error before allocating.
    let (mut client, mut server) = duplex();
    block_on(async {
        let prefix = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes();
        client.write_all(&prefix).await.unwrap();
        client.flush().await.unwrap();

        let error = read_frame(&mut server).await.unwrap_err().to_string();
        assert!(error.contains("exceeds"), "{error}");

        // No payload was consumed and nothing panicked: the stream still works.
        let good = Frame::Subscribe { request_id: None };
        write_frame(&mut client, &good).await.unwrap();
        assert_eq!(read_frame(&mut server).await.unwrap(), good);
    });

    // Write side: encoding something past the cap errors rather than emitting
    // a frame the peer is required to reject.
    let (mut client, _server) = duplex();
    block_on(async {
        let huge = Frame::Error {
            session_id: None,
            workspace_id: None,
            code: error_code::INTERNAL.into(),
            message: "x".repeat(MAX_FRAME_BYTES + 1),
            request_id: None,
        };
        let error = write_frame(&mut client, &huge)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeds"), "{error}");
    });
}

#[test]
fn unknown_fields_are_ignored_for_forward_compatibility() {
    // A newer peer adds a top-level key and a body field; a peer at this
    // generation must ignore both rather than fail the frame (§2, §4).
    let json = br#"{
        "op": "status",
        "trace": {"span": "abc"},
        "body": {
            "session_id": "s-1",
            "status": "needs_input",
            "since": 1754200100,
            "reason": "bell",
            "future_nested": { "a": [1, 2, 3] }
        }
    }"#;

    let (mut client, mut server) = duplex();
    let frame = block_on(async {
        client.write_all(&framed(json)).await.unwrap();
        client.flush().await.unwrap();
        read_frame(&mut server).await.unwrap()
    });

    assert_eq!(
        frame,
        Frame::Status {
            session_id: SessionId::new("s-1"),
            status: SessionStatus::NeedsInput,
            since: 1_754_200_100,
        }
    );

    // Absent optional fields fall back to `None` rather than failing.
    let json = br#"{"op": "exited", "body": {"session_id": "s-1"}}"#;
    let frame = block_on(async {
        client.write_all(&framed(json)).await.unwrap();
        client.flush().await.unwrap();
        read_frame(&mut server).await.unwrap()
    });

    assert_eq!(
        frame,
        Frame::Exited {
            session_id: SessionId::new("s-1"),
            exit_code: None,
        }
    );
}

/// The repeal. An op this build has never heard of used to be a decode error
/// that took the connection — and every attach on it — with it. It is now
/// scoped to the one request: the failure names the op and echoes the `rid` so
/// the sender can be answered, and the very next frame on the same stream
/// decodes normally.
#[test]
fn an_unknown_op_fails_one_request_and_the_stream_survives() {
    let (mut client, mut server) = duplex();
    let next = Frame::RenameWorkspace {
        workspace_id: "w-1".into(),
        name: "spike".into(),
        request_id: None,
    };

    block_on(async {
        client
            .write_all(&framed(br#"{"op":"lease_renew","rid":9,"body":{}}"#))
            .await
            .unwrap();
        client.flush().await.unwrap();
        write_frame(&mut client, &next).await.unwrap();

        match read_frame(&mut server).await.unwrap_err() {
            ReadFrameError::UnknownOp { op, rid } => {
                assert_eq!(op, "lease_renew");
                assert_eq!(rid, Some(9));
            }
            other => panic!("expected UnknownOp, got {other:?}"),
        }

        assert_eq!(
            read_frame(&mut server).await.unwrap(),
            next,
            "one unknown op must not cost the connection"
        );
    });
}

/// §2 bounds `op` to `[a-z0-9_]`, ≤ 64 bytes. A string outside that grammar can
/// never name an operation — op identifiers are permanent — so it is
/// `unknown_op`, request-scoped like any other unknown op, and never a reason to
/// close the connection.
#[test]
fn an_op_outside_the_grammar_is_an_unknown_op() {
    let long = "a".repeat(MAX_OP_BYTES + 1);
    let cases: [(&str, String); 6] = [
        ("uppercase", "Attach".to_owned()),
        ("a space", "lease renew".to_owned()),
        ("punctuation", "lease-renew".to_owned()),
        ("not ascii", "attaché".to_owned()),
        ("empty", String::new()),
        ("past 64 bytes", long),
    ];

    for (name, op) in cases {
        let payload = json!({ "op": op, "rid": 9, "body": {} }).to_string();
        match decode_frame(payload.as_bytes()) {
            Err(error @ ReadFrameError::UnknownOp { rid: Some(9), .. }) => {
                assert!(
                    error.is_request_scoped(),
                    "{name}: an ill-formed op costs one request, not the connection"
                );
            }
            other => panic!("{name}: expected UnknownOp, got {other:?}"),
        }
    }

    // The bound is on the grammar, not on recognition: 64 bytes of legal
    // characters is a well-formed op this build simply does not implement, and
    // it lands in the same place for the ordinary reason.
    let payload = json!({ "op": "a".repeat(MAX_OP_BYTES), "rid": 9, "body": {} }).to_string();
    assert!(matches!(
        decode_frame(payload.as_bytes()),
        Err(ReadFrameError::UnknownOp { .. })
    ));
}

/// A rejection is a frame, and [`write_frame`] enforces `MAX_FRAME_BYTES` on it
/// — so quoting the peer's own `op` back at it unbounded made a crafted request
/// produce a reply twice its size that could not be written at all, breaking the
/// writer task and every attach on the connection. Nothing peer-derived reaches
/// an error `message` unbounded.
#[test]
fn a_huge_op_yields_a_small_rejection() {
    let op = "a".repeat(1024 * 1024);
    let payload = json!({ "op": op, "rid": 9, "body": {} }).to_string();
    let error = decode_frame(payload.as_bytes()).expect_err("a megabyte-long op is not an op");
    assert!(matches!(
        error,
        ReadFrameError::UnknownOp { rid: Some(9), .. }
    ));

    let reply = rejection_frame(&error).expect("an unknown op with a rid is answered");
    let encoded = encode_frame(&reply).expect("encoding the rejection");
    assert!(
        encoded.len() < 1024,
        "the reply quoted the request back: {} bytes",
        encoded.len()
    );
    assert!(encoded.len() < MAX_FRAME_BYTES);
    match reply {
        Frame::Error { code, message, .. } => {
            assert_eq!(code, error_code::UNKNOWN_OP);
            assert!(message.ends_with("…\""), "expected an elided op: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

/// Stage one is the envelope alone and must not depend on `op`. Everything it
/// can fail on is a protocol violation the receiver answers `malformed_frame`
/// and MAY close over — the only decode failure in the contract that may.
#[test]
fn a_broken_envelope_is_always_a_malformed_frame() {
    let cases: [(&str, &[u8]); 6] = [
        ("payload is not an object", br#"[1, 2, 3]"#),
        ("no op", br#"{"body":{}}"#),
        ("no body", br#"{"op":"subscribe"}"#),
        ("body is not an object", br#"{"op":"subscribe","body":7}"#),
        (
            "rid is a string",
            br#"{"op":"subscribe","rid":"9","body":{}}"#,
        ),
        (
            "rid is negative",
            br#"{"op":"subscribe","rid":-1,"body":{}}"#,
        ),
    ];

    for (name, payload) in cases {
        match decode_frame(payload) {
            Err(ReadFrameError::MalformedFrame { .. }) => {}
            other => panic!("{name}: expected MalformedFrame, got {other:?}"),
        }
    }
}

/// Stage two's failure is request-scoped and carries the `rid` back, so the
/// sender learns which of its in-flight requests died.
#[test]
fn a_broken_body_is_request_scoped_and_echoes_its_rid() {
    let payload = br#"{"op":"attach","rid":9,"body":{"session_id":42}}"#;
    match decode_frame(payload) {
        Err(ReadFrameError::MalformedBody { op, rid, detail }) => {
            assert_eq!(op, "attach");
            assert_eq!(rid, Some(9));
            assert!(!detail.is_empty(), "the failure must say what broke");
        }
        other => panic!("expected MalformedBody, got {other:?}"),
    }
}

/// The wire-side classifier, pinned against the spec's own list (§2, §3.3). A
/// sender that gets these back has lost one request; a sender that gets
/// anything else back has lost the stream, and confusing the two is how a
/// rejected `resize` takes a live terminal with it.
#[test]
fn only_the_codes_that_refuse_one_frame_are_request_scoped() {
    for code in [
        error_code::MALFORMED_BODY,
        error_code::UNKNOWN_OP,
        // Nothing emits this yet — generation 2 advertises no capabilities —
        // but §3.3 already says its receiver keeps serving.
        error_code::UNCAPABLE_PEER,
    ] {
        assert!(error_code::is_request_scoped(code), "{code}");
    }
    for code in [
        // The envelope itself was unreadable, so the length prefix is no longer
        // trustworthy and there is nothing left to keep serving.
        error_code::MALFORMED_FRAME,
        error_code::UNSUPPORTED_GENERATION,
        // These are about a session or the daemon, not about one frame we sent.
        error_code::NOT_FOUND,
        error_code::STALE_REV,
        error_code::INVALID_ARGUMENT,
        error_code::PERSIST_FAILED,
        error_code::DECLINED,
        error_code::INTERNAL,
        // §2.1: an unrecognised code is a generic failure, not a licence to
        // assume the stream survived.
        "a_code_from_a_later_generation",
    ] {
        assert!(!error_code::is_request_scoped(code), "{code}");
    }
}

/// What may be answered, and what may only be logged. A request-scoped failure
/// with no `rid` could not be correlated to anything on the far side and named
/// no session, so there is nobody to answer: it is dropped, deliberately.
#[test]
fn rejection_frames_answer_only_what_can_be_correlated() {
    let malformed = rejection_frame(&ReadFrameError::MalformedFrame {
        detail: "no op".into(),
    })
    .expect("a malformed frame is always answered");
    assert!(matches!(
        &malformed,
        Frame::Error { code, request_id, .. }
            if code == error_code::MALFORMED_FRAME && request_id.is_none()
    ));

    let unknown = rejection_frame(&ReadFrameError::UnknownOp {
        op: "lease_renew".into(),
        rid: Some(9),
    })
    .expect("an unknown op with a rid is answered");
    assert!(matches!(
        &unknown,
        Frame::Error { code, request_id, .. }
            if code == error_code::UNKNOWN_OP && *request_id == Some(9)
    ));

    let body = rejection_frame(&ReadFrameError::MalformedBody {
        op: "attach".into(),
        rid: Some(4),
        detail: "invalid type".into(),
    })
    .expect("a malformed body with a rid is answered");
    assert!(matches!(
        &body,
        Frame::Error { code, request_id, .. }
            if code == error_code::MALFORMED_BODY && *request_id == Some(4)
    ));

    assert!(
        rejection_frame(&ReadFrameError::UnknownOp {
            op: "lease_renew".into(),
            rid: None,
        })
        .is_none(),
        "log and drop: there is nobody to answer"
    );
    assert!(
        rejection_frame(&ReadFrameError::MalformedBody {
            op: "write".into(),
            rid: None,
            detail: "invalid type".into(),
        })
        .is_none(),
        "log and drop: there is nobody to answer"
    );
    assert!(
        rejection_frame(&ReadFrameError::Transport(anyhow::anyhow!("eof"))).is_none(),
        "there is nothing left to write to"
    );
}

/// The pre-cut signature (§6.1): a daemon that predates the envelope cannot
/// decode `hello`, drops the connection without writing anything, and the
/// client sees EOF where it expected an ack. Only a transport failure can be
/// that — anything that decoded far enough to be malformed did answer.
#[test]
fn a_handshake_that_ends_in_eof_is_recognisable() {
    let (_client, mut server) = duplex();
    let error = block_on(async {
        drop(_client);
        read_frame(&mut server).await.unwrap_err()
    });
    assert!(
        ade_session::client::is_handshake_eof(&error),
        "expected the pre-cut EOF signature, got {error:?}"
    );

    assert!(!ade_session::client::is_handshake_eof(
        &ReadFrameError::MalformedFrame {
            detail: "no op".into(),
        }
    ));
    assert!(
        ade_session::client::PRE_CUT_DIAGNOSIS.contains("predates the protocol cut"),
        "the diagnosis is what a user reads instead of an IO error"
    );
}

/// `G = min(maxes)`, valid only when it clears both minima. The daemon computes
/// it; both ends of these cases are the same function, so the client's
/// verification and the daemon's selection cannot disagree.
#[test]
fn generation_selection_takes_the_lower_max_and_fails_on_disjoint_ranges() {
    // Client [2,2] against daemon [2,3]: the client's max is the ceiling.
    assert_eq!(select_generation(2, 2, 2, 3), Some(2));
    // Client [2,3] against daemon [2,2]: the daemon's max is.
    assert_eq!(select_generation(2, 3, 2, 2), Some(2));
    // A client that has moved on entirely: nothing overlaps, so nothing is
    // served — the one negotiation outcome that is fatal by design.
    assert_eq!(select_generation(3, 4, 2, 2), None);
    // Equal ranges settle on the generation both sides speak.
    assert_eq!(select_generation(2, 2, 2, 2), Some(2));
    assert_eq!(
        select_generation(
            MIN_GENERATION,
            MAX_GENERATION,
            MIN_GENERATION,
            MAX_GENERATION
        ),
        Some(MAX_GENERATION),
        "this build must be able to talk to itself"
    );
}

/// Duplicates are an encoding artefact with one unambiguous reading, and an
/// identifier the other side has never heard of is not an error — it simply
/// falls out of the intersection.
#[test]
fn capabilities_deduplicate_and_intersect() {
    let mine: Vec<String> = ["ade.layout", "ade.layout", "ade.persist"]
        .map(String::from)
        .to_vec();
    let theirs: Vec<String> = ["ade.persist", "ade.something_new", "ade.persist"]
        .map(String::from)
        .to_vec();

    let effective = effective_capabilities(&mine, &theirs);
    assert_eq!(
        effective.into_iter().collect::<Vec<_>>(),
        vec!["ade.persist".to_owned()],
        "a duplicate still says the peer is capable, and an unknown id just falls out"
    );

    assert!(effective_capabilities(&mine, &[]).is_empty());
    assert!(validate_capabilities(&mine).is_ok(), "duplicates are legal");
}

/// The bounds are fatal to the handshake, so the message has to name the
/// offender: whoever reads it is looking at a log line, not at the list.
#[test]
fn capability_lists_past_their_bounds_are_rejected() {
    let too_many: Vec<String> = (0..257).map(|i| format!("cap{i}")).collect();
    let error = validate_capabilities(&too_many).unwrap_err();
    assert!(error.contains("257"), "{error}");
    assert!(validate_capabilities(&too_many[..256]).is_ok());

    let too_long = vec!["a".repeat(65)];
    let error = validate_capabilities(&too_long).unwrap_err();
    assert!(error.contains("65 bytes"), "{error}");
    assert!(validate_capabilities(&["a".repeat(64)]).is_ok());

    let shouting = vec!["ade.Layout".to_owned()];
    let error = validate_capabilities(&shouting).unwrap_err();
    assert!(error.contains("ade.Layout"), "{error}");
    assert!(validate_capabilities(&["ade.layout-2_0".to_owned()]).is_ok());
}

/// The same amplification as [`a_huge_op_yields_a_small_rejection`], one frame
/// earlier. This reason becomes the handshake's `invalid_argument` message
/// (`crates/ade_session_daemon/src/server.rs`), and the capability that trips
/// the length bound is by definition the one that must not be quoted whole: at
/// 65 bytes it costs nothing, at 4 MB the Debug escaping and the JSON escaping
/// between them push the refusal past `MAX_FRAME_BYTES`, `write_frame` refuses
/// it, and the handshake dies unanswered instead of being cleanly rejected.
/// The 65-byte case above is why this went unnoticed.
#[test]
fn a_huge_capability_yields_a_small_rejection() {
    // Quotes, because Debug doubles every one of them: the bound has to hold
    // before the escaping, not after.
    let enormous = vec!["\"".repeat(4 * 1024 * 1024)];
    let reason = validate_capabilities(&enormous).expect_err("4 MB is not a capability");
    assert!(reason.contains("4194304 bytes"), "{reason}");
    assert!(
        reason.contains('…'),
        "expected an elided capability: {reason}"
    );

    let refusal = Frame::Error {
        session_id: None,
        workspace_id: None,
        code: error_code::INVALID_ARGUMENT.to_owned(),
        message: format!("unusable capability list: {reason}"),
        request_id: Some(1),
    };
    let encoded = encode_frame(&refusal).expect("encoding the refusal");
    assert!(
        encoded.len() < 1024,
        "the refusal quoted the capability back: {} bytes",
        encoded.len()
    );
    assert!(encoded.len() < MAX_FRAME_BYTES);
}

#[test]
fn a_layout_document_without_a_schema_version_reads_as_the_current_one() {
    let json =
        br#"{"root": {"type": "Leaf", "tabs": [{"type": "Terminal", "session_id": "s-1"}]}}"#;
    let layout: LayoutDoc = serde_json::from_slice(json).expect("decoding a layout");
    assert_eq!(layout.schema_version, LAYOUT_SCHEMA_VERSION);
    assert_eq!(layout.terminal_sessions(), vec![SessionId::new("s-1")]);
}

#[test]
fn terminal_sessions_are_listed_in_tree_order_and_editors_are_ignored() {
    assert_eq!(
        nested_layout().terminal_sessions(),
        vec![
            SessionId::new("s-1"),
            SessionId::new("s-2"),
            SessionId::new("s-3"),
        ]
    );
}

/// Pruning is what a daemon restart does to a stored layout: the tabs whose
/// sessions are gone go, the editor tabs and the structure around them stay,
/// and a split that loses a whole child collapses into the other one.
#[test]
fn pruning_drops_dead_terminals_clamps_active_and_collapses_splits() {
    let mut layout = nested_layout();
    // Only the second leaf's first terminal survives.
    assert!(layout.retain_sessions(|id| id.as_str() == "s-2"));
    assert_eq!(layout.terminal_sessions(), vec![SessionId::new("s-2")]);
    let LayoutNode::Split { children, .. } = &layout.root else {
        panic!("the editor tab should have kept the split alive: {layout:?}");
    };
    // The first leaf keeps its editor tab; the second is clamped back onto its
    // one remaining tab.
    assert_eq!(
        children[0],
        LayoutNode::leaf(vec![Tab::Editor {
            path: "/home/u/proj/src/main.rs".into(),
        }])
    );
    assert_eq!(
        children[1],
        LayoutNode::Leaf {
            tabs: vec![Tab::Terminal {
                session_id: SessionId::new("s-2"),
            }],
            active: 0,
            focused: true,
        }
    );

    // Nothing survives: the split collapses all the way to an empty leaf, and
    // a second prune has nothing left to change.
    let mut layout = LayoutDoc::new(LayoutNode::Split {
        dir: SplitDir::Vertical,
        ratio: 0.5,
        children: Box::new([
            LayoutNode::leaf(vec![Tab::Terminal {
                session_id: SessionId::new("s-1"),
            }]),
            LayoutNode::leaf(vec![Tab::Terminal {
                session_id: SessionId::new("s-2"),
            }]),
        ]),
    });
    assert!(layout.retain_sessions(|_| false));
    assert_eq!(layout, LayoutDoc::empty());
    assert!(!layout.retain_sessions(|_| false));
}

/// A split that loses one child becomes the other child, rather than a split
/// with a hole in it.
#[test]
fn a_split_with_one_surviving_child_becomes_that_child() {
    let survivor = LayoutNode::leaf(vec![Tab::Terminal {
        session_id: SessionId::new("s-2"),
    }]);
    let mut layout = LayoutDoc::new(LayoutNode::Split {
        dir: SplitDir::Vertical,
        ratio: 0.5,
        children: Box::new([
            LayoutNode::leaf(vec![Tab::Terminal {
                session_id: SessionId::new("s-1"),
            }]),
            survivor.clone(),
        ]),
    });
    assert!(layout.retain_sessions(|id| id.as_str() == "s-2"));
    assert_eq!(layout.root, survivor);
}

// ------------------------------------------- the frozen generation-2 wire ---
//
// Hand-derived from the types at `be8f183cf0`, the envelope cut this build
// still serves as the previous generation. These bytes are the contract with
// every daemon and client already deployed at 2: a change here is a change to
// the protocol, not to a test.

/// The gen-2 `create_workspace` carried the first session's environment and
/// size, because at 2 the op *was* the combined create. All three fields must
/// still decode, or an old client's request arrives with its shell's
/// environment silently dropped.
#[test]
fn the_generation_two_create_workspace_still_decodes_whole() {
    let wire = br#"{"op":"create_workspace","rid":5,"body":{"root":"/home/u/proj","name":"proj","env":[["TERM","xterm-256color"],["ADE_TEST","1"]],"cols":120,"rows":40}}"#;
    assert_eq!(
        decode_frame(wire).unwrap(),
        Frame::CreateWorkspace {
            root: "/home/u/proj".into(),
            name: Some("proj".into()),
            project_id: None,
            project_identity: None,
            env: vec![
                ("TERM".into(), "xterm-256color".into()),
                ("ADE_TEST".into(), "1".into()),
            ],
            cols: Some(120),
            rows: Some(40),
            request_id: Some(5),
        }
    );
}

/// A gen-2 `attach` names a session and nothing else. Both directions matter:
/// the old peer's frame must decode with no view, and a frame bound for one
/// must not grow a `view_id` field it has never heard of.
#[test]
fn the_generation_two_attach_has_no_view() {
    let wire = br#"{"op":"attach","rid":7,"body":{"session_id":"s-1"}}"#;
    let attach = Frame::Attach {
        session_id: SessionId::new("s-1"),
        view_id: None,
        request_id: Some(7),
    };
    assert_eq!(decode_frame(wire).unwrap(), attach);

    let encoded = encode_frame(&attach).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&encoded).unwrap(),
        serde_json::from_slice::<Value>(wire).unwrap(),
        "encoded {}",
        String::from_utf8_lossy(&encoded)
    );
}

/// The gen-2 handshake pair, pinned at 2..=2 — the range a daemon from the cut
/// advertises. Its ack predates `binary_hash`, `upgrade_ready` and
/// `instance_id`, and all three must read as absent rather than fail.
#[test]
fn the_generation_two_handshake_pair_decodes() {
    let hello = br#"{"op":"hello","rid":1,"body":{"min_generation":2,"max_generation":2,"capabilities":[]}}"#;
    assert_eq!(
        decode_frame(hello).unwrap(),
        Frame::Hello(Hello {
            min_generation: 2,
            max_generation: 2,
            capabilities: Vec::new(),
            request_id: Some(1),
        })
    );

    let ack = br#"{"op":"hello_ack","rid":1,"body":{"daemon_version":"0.1.0","protocol_version":2,"host_os":"linux","min_generation":2,"max_generation":2,"generation":2,"capabilities":[],"degraded":false}}"#;
    assert_eq!(
        decode_frame(ack).unwrap(),
        Frame::HelloAck(HelloAck {
            daemon_version: "0.1.0".into(),
            protocol_version: 2,
            host_os: "linux".into(),
            min_generation: 2,
            max_generation: 2,
            generation: 2,
            capabilities: Vec::new(),
            degraded: false,
            binary_hash: None,
            upgrade_ready: None,
            instance_id: None,
            request_id: Some(1),
        })
    );
}

/// Gen-2 mutation acks have no `persisted` field, and its absence means the
/// mutation *was* recorded (§8.5). The encoder's half of the same contract:
/// `persisted: true` is omitted, so a frame bound for an old peer keeps the
/// shape it has always had.
#[test]
fn generation_two_acks_omit_persisted_and_read_as_persisted() {
    let cases: Vec<(&[u8], Frame)> = vec![
        (
            br#"{"op":"created","rid":4,"body":{"session":{"id":"s-1","workspace_id":"ws-1","agent_kind":"claude","instance_label":"main","cwd":"/home/u/proj","created_at":1754200000,"status":"working"}}}"#,
            Frame::Created {
                session: session_info("s-1"),
                persisted: true,
                request_id: Some(4),
            },
        ),
        (
            br#"{"op":"workspace_removed","rid":8,"body":{"workspace_id":"w-1"}}"#,
            Frame::WorkspaceRemoved {
                workspace_id: "w-1".into(),
                persisted: true,
                request_id: Some(8),
            },
        ),
    ];
    for (wire, expected) in cases {
        assert_eq!(
            decode_frame(wire).unwrap(),
            expected,
            "decoding {}",
            String::from_utf8_lossy(wire)
        );
        let encoded = encode_frame(&expected).unwrap();
        let value: Value = serde_json::from_slice(&encoded).unwrap();
        assert!(
            !value["body"].as_object().unwrap().contains_key("persisted"),
            "encoded {}",
            String::from_utf8_lossy(&encoded)
        );
        assert_eq!(
            value,
            serde_json::from_slice::<Value>(wire).unwrap(),
            "the re-encoded frame drifted from the frozen shape"
        );
    }
}

/// The sliding window, both directions: whichever peer is the older one, the
/// pair settles on 2 and nobody is refused.
#[test]
fn the_two_generation_window_meets_a_pinned_generation_two_peer() {
    assert_eq!(
        select_generation(MIN_GENERATION, MAX_GENERATION, 2, 2),
        Some(2),
        "this build against a daemon from the cut"
    );
    assert_eq!(
        select_generation(2, 2, MIN_GENERATION, MAX_GENERATION),
        Some(2),
        "a client from the cut against this build"
    );
}

/// A hello pinned narrower than this build can serve.
fn pinned_hello(min: u32, max: u32) -> Hello {
    Hello {
        min_generation: min,
        max_generation: max,
        capabilities: Vec::new(),
        request_id: Some(1),
    }
}

/// A caller that pinned a range narrower than this build can serve must be held
/// to the range it *sent*: verifying against the crate constants would accept a
/// generation this connection never offered, and the frame after it would
/// decode as nonsense.
#[test]
fn handshake_rejects_an_ack_outside_the_range_the_hello_offered() {
    let (client_stream, mut server) = duplex();
    let mut connection = Connection::new(client_stream);

    let error = block_on(async {
        let fake_daemon = async {
            match read_frame(&mut server).await.unwrap() {
                Frame::Hello(hello) => {
                    assert_eq!((hello.min_generation, hello.max_generation), (2, 2));
                }
                other => panic!("expected Hello, got {other:?}"),
            }
            write_frame(&mut server, &Frame::HelloAck(hello_ack()))
                .await
                .unwrap();
        };
        futures::future::join(fake_daemon, connection.handshake(pinned_hello(2, 2)))
            .await
            .1
    })
    .expect_err("an ack above the offered range must not be accepted");

    let error = format!("{error:#}");
    assert!(
        error.contains("2..=2"),
        "the refusal must name the range this connection offered: {error}"
    );
}

/// The same handshake, accepted: an ack inside the pinned range is what a
/// two-generation daemon answers a pinned client with.
#[test]
fn handshake_accepts_an_ack_inside_the_pinned_range() {
    let (client_stream, mut server) = duplex();
    let mut connection = Connection::new(client_stream);

    let ack = block_on(async {
        let fake_daemon = async {
            read_frame(&mut server).await.unwrap();
            write_frame(
                &mut server,
                &Frame::HelloAck(HelloAck {
                    generation: 2,
                    protocol_version: 2,
                    ..hello_ack()
                }),
            )
            .await
            .unwrap();
        };
        futures::future::join(fake_daemon, connection.handshake(pinned_hello(2, 2)))
            .await
            .1
    })
    .expect("a generation inside the offered range");
    assert_eq!(ack.generation, 2);
}
