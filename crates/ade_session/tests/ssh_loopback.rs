//! The ssh half of the transport, driven against **real** OpenSSH over
//! loopback: `ssh localhost` with a dedicated test key.
//!
//! Every test is gated on that connection working, once, and skips otherwise —
//! a machine without a listening sshd (or without the key) must still get a
//! green `cargo test`. Nothing here mocks ssh: the point is the flags, the
//! quoting and the streamlocal forward as OpenSSH actually parses them.
//!
//! The daemon-dependent test needs a built `ade-daemon`, which this crate
//! cannot name (`CARGO_BIN_EXE_*` only covers your own binaries), so it reads
//! `ADE_TEST_DAEMON_BIN` and skips when it is unset. The same ground is covered
//! end to end in `ade_session_daemon/tests/ssh_remote.rs`, which can.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::OnceLock;

use ade_session::deploy::{DeployConfig, DeployOutcome, HostExec, Version, ensure_daemon};
use ade_session::ssh::{EnsureOutcome, HostForward, LocalEndpoint, SshHost, ensure_remote_daemon};
use tempfile::TempDir;

/// The key this box's sshd trusts for loopback tests.
fn test_key() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME"))
        .join(".ssh")
        .join("id_ed25519_ade_test")
}

/// `localhost` over ssh, or `None` (having said so) if that is not available
/// here.
///
/// The probe runs at most once per test binary: it is a real ssh handshake, and
/// paying for it in every test would be the slowest thing in the suite.
fn loopback() -> Option<SshHost> {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();

    let host = SshHost::new("localhost").with_extra_args([
        "-i".to_owned(),
        test_key().display().to_string(),
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

/// A built `ade-daemon`, if the caller pointed at one.
fn daemon_binary() -> Option<String> {
    match std::env::var("ADE_TEST_DAEMON_BIN") {
        Ok(path) if !path.is_empty() => Some(path),
        _ => {
            eprintln!("skipping: ADE_TEST_DAEMON_BIN is not set");
            None
        }
    }
}

fn argv<'a>(parts: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    parts.into_iter().map(str::to_owned).collect()
}

/// Held for as long as a test owns a reserved loopback port.
///
/// `LocalEndpoint::loopback` reserves a port by binding `:0` and releasing it
/// again, so two reservations racing inside one process can be handed the *same*
/// port — the kernel is free to re-issue what nobody holds — and then one ssh
/// binds it and the other exits. Production has one forward per host and lives
/// with that window; a test suite must not be flaky, so here the reservation
/// and the bind that follows it are kept apart.
fn reserved_port() -> std::sync::MutexGuard<'static, ()> {
    static PORTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    PORTS.lock().unwrap_or_else(|error| error.into_inner())
}

/// Arguments arrive on the far side exactly as they left, which is the whole
/// job of the shell quoting: ssh joins everything after the destination with
/// spaces and a login shell re-splits it.
#[test]
fn run_carries_argv_through_the_remote_shell_intact() {
    let Some(host) = loopback() else { return };

    let output = host
        .run(&argv([
            "printf",
            "[%s]",
            "a b",
            "it's",
            "",
            "ünïcøde ✓",
            "$HOME",
        ]))
        .expect("running printf over ssh");

    assert!(output.success(), "printf failed: {}", output.stderr);
    assert_eq!(output.stdout, "[a b][it's][][ünïcøde ✓][$HOME]");
}

/// A command that is not there is exit 127, not a spawn error — which is what
/// `ensure_daemon`'s probe and `ensure_remote_daemon` both key off.
#[test]
fn a_missing_remote_binary_is_exit_127() {
    let Some(host) = loopback() else { return };

    let output = host
        .run(&argv(["/nonexistent/ade-daemon", "--version"]))
        .expect("ssh itself ran");

    assert_eq!(output.exit_code, 127);
}

#[test]
fn upload_writes_the_bytes_executable_and_leaves_no_temp_file() {
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
    assert!(
        !path.with_extension("ade-upload").exists(),
        "the temp file was renamed, not left behind"
    );
}

/// Deployment is transport-free by construction, so the interesting part is
/// that `SshHost` satisfies the same contract `LocalHost` does: install when
/// nothing is there, then leave it alone.
#[test]
fn ensure_daemon_deploys_over_ssh_and_then_keeps_its_hands_off() {
    let Some(host) = loopback() else { return };
    let dir = TempDir::new().expect("temp dir");
    let binary = b"#!/bin/sh\nprintf 'ade_session_daemon 0.2.0\\n'\n".to_vec();
    let config = DeployConfig::new(binary, Version::new(0, 2, 0))
        .with_bin_path(
            dir.path()
                .join("bin")
                .join("ade-daemon")
                .display()
                .to_string(),
        )
        .with_socket_path(dir.path().join("daemon.sock").display().to_string())
        .with_state_dir(dir.path().join("state").display().to_string());

    let installed = ensure_daemon(&host, &config).expect("first deployment");
    assert_eq!(installed.outcome, DeployOutcome::Installed);

    let second = ensure_daemon(&host, &config).expect("second deployment");
    assert_eq!(
        second.outcome,
        DeployOutcome::AlreadyCurrent {
            version: Version::new(0, 2, 0)
        }
    );
}

/// `ensure_daemon` expands `~` against the host's own `$HOME`, asked for over
/// this same channel. `ssh host command` is not a login shell, so that `$HOME`
/// is set at all is the thing worth checking on a real connection.
#[test]
fn a_non_interactive_ssh_command_still_knows_its_home() {
    let Some(host) = loopback() else { return };

    let output = host
        .run(&argv(["sh", "-c", "printf %s \"$HOME\""]))
        .expect("asking for $HOME over ssh");

    assert!(output.success(), "{}", output.stderr);
    assert_eq!(output.stdout, std::env::var("HOME").expect("HOME"));
}

/// The forward is a byte pipe to a Unix socket on the far side, and every
/// connection to the local socket is its own channel on the one ssh process.
#[test]
fn the_forward_carries_connections_to_the_remote_socket() {
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::{UnixListener, UnixStream};

    let Some(host) = loopback() else { return };
    let dir = TempDir::new().expect("temp dir");
    let remote = dir.path().join("remote.sock");
    let local = dir.path().join("local.sock");

    // Stands in for the daemon: answers two connections, so the test can prove
    // two channels ride the same forward. Connections that say nothing are
    // skipped rather than counted — `establish`'s readiness probe is one of
    // them, and a daemon treats it the same way.
    let listener = UnixListener::bind(&remote).expect("binding the far-side socket");
    let server = std::thread::spawn(move || {
        let mut served = 0;
        while served < 2 {
            let (mut stream, _) = listener.accept().expect("accepting");
            let mut request = [0u8; 5];
            if stream.read_exact(&mut request).is_err() {
                continue;
            }
            stream
                .write_all(&request.to_ascii_uppercase())
                .expect("answering");
            served += 1;
        }
    });

    let mut forward = HostForward::establish(
        &host,
        &remote.display().to_string(),
        LocalEndpoint::Socket(local.clone()),
    )
    .expect("establishing the forward");
    assert!(forward.is_alive());
    assert_eq!(forward.local(), &LocalEndpoint::Socket(local.clone()));

    for _ in 0..2 {
        let mut channel = UnixStream::connect(&local).expect("connecting through the forward");
        channel.write_all(b"hello").expect("writing");
        let mut answer = [0u8; 5];
        channel.read_exact(&mut answer).expect("reading");
        assert_eq!(&answer, b"HELLO");
    }
    server.join().expect("the far side served both channels");

    drop(forward);
    assert!(
        UnixStream::connect(&local).is_err(),
        "dropping the forward takes the local socket with it"
    );
}

/// The Windows client's forward, proven on Linux because Linux is the only
/// place ADE has tests: a loopback *port* on this side, the same Unix socket on
/// the far side. Only the client's end differs — the AF_UNIX end belongs to the
/// remote sshd, which is why this shape works from a client that cannot bind a
/// Unix socket at all.
#[test]
fn a_loopback_tcp_forward_carries_connections_to_the_remote_socket() {
    use std::io::{Read as _, Write as _};
    use std::net::TcpStream;
    use std::os::unix::net::UnixListener;

    let Some(host) = loopback() else { return };
    let dir = TempDir::new().expect("temp dir");
    let remote = dir.path().join("remote.sock");

    // Same stand-in daemon as the socket-mode test: two answered channels, and
    // silent connections (the readiness probe is one) skipped rather than
    // counted.
    let listener = UnixListener::bind(&remote).expect("binding the far-side socket");
    let server = std::thread::spawn(move || {
        let mut served = 0;
        while served < 2 {
            let (mut stream, _) = listener.accept().expect("accepting");
            let mut request = [0u8; 5];
            if stream.read_exact(&mut request).is_err() {
                continue;
            }
            stream
                .write_all(&request.to_ascii_uppercase())
                .expect("answering");
            served += 1;
        }
    });

    let _ports = reserved_port();
    let endpoint = LocalEndpoint::loopback().expect("reserving a port");
    let LocalEndpoint::Loopback(port) = endpoint else {
        panic!("expected a loopback endpoint");
    };
    let mut forward =
        HostForward::establish(&host, &remote.display().to_string(), endpoint.clone())
            .expect("establishing the tcp forward");
    assert!(forward.is_alive());
    assert_eq!(forward.local(), &endpoint);

    for _ in 0..2 {
        let mut channel =
            TcpStream::connect(("127.0.0.1", port)).expect("connecting through the forward");
        channel.write_all(b"hello").expect("writing");
        let mut answer = [0u8; 5];
        channel.read_exact(&mut answer).expect("reading");
        assert_eq!(&answer, b"HELLO");
    }
    server.join().expect("the far side served both channels");

    drop(forward);
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "dropping the forward releases the loopback port"
    );
}

/// The same caveat the socket-mode forward has, and it is the reason
/// `--ensure` must run before the forward is trusted: `ExitOnForwardFailure`
/// only covers the local bind, so a port forward to a socket nobody has bound
/// comes up fine and fails one connection at a time.
#[test]
fn a_tcp_forward_to_a_missing_remote_socket_establishes_and_fails_per_channel() {
    use std::io::Read as _;
    use std::net::TcpStream;

    let Some(host) = loopback() else { return };
    let dir = TempDir::new().expect("temp dir");

    let _ports = reserved_port();
    let endpoint = LocalEndpoint::loopback().expect("reserving a port");
    let LocalEndpoint::Loopback(port) = endpoint else {
        panic!("expected a loopback endpoint");
    };
    let mut forward = HostForward::establish(
        &host,
        &dir.path()
            .join("nothing-is-here.sock")
            .display()
            .to_string(),
        endpoint,
    )
    .expect("the local bind is all ssh checks up front");

    assert!(forward.is_alive(), "the forward itself is fine");
    let mut channel = TcpStream::connect(("127.0.0.1", port)).expect("the local port accepts");
    let mut answer = Vec::new();
    channel
        .read_to_end(&mut answer)
        .expect("reading the channel");
    assert!(answer.is_empty(), "the far side has nothing to connect to");
}

/// What a forward to a socket nobody has bound actually does — worth pinning,
/// because it is not what `ExitOnForwardFailure=yes` suggests. ssh binds the
/// *local* socket at startup and only reaches for the remote one per channel,
/// so establishing succeeds and each connection fails on its own with EOF.
#[test]
fn a_forward_to_a_missing_remote_socket_establishes_and_fails_per_channel() {
    use std::io::Read as _;
    use std::os::unix::net::UnixStream;

    let Some(host) = loopback() else { return };
    let dir = TempDir::new().expect("temp dir");
    let local = dir.path().join("local.sock");

    let mut forward = HostForward::establish(
        &host,
        &dir.path()
            .join("nothing-is-here.sock")
            .display()
            .to_string(),
        LocalEndpoint::Socket(local.clone()),
    )
    .expect("the local bind is all ssh checks up front");

    assert!(forward.is_alive(), "the forward itself is fine");
    let mut channel = UnixStream::connect(&local).expect("the local socket accepts");
    let mut answer = Vec::new();
    channel
        .read_to_end(&mut answer)
        .expect("reading the channel");
    assert!(answer.is_empty(), "the far side has nothing to connect to");
}

/// A forward that ssh itself refuses — here, an unusable local path — comes
/// back as an error carrying ssh's own words, not as a live process with
/// nothing behind it.
#[test]
fn a_forward_ssh_refuses_reports_its_stderr() {
    let Some(host) = loopback() else { return };
    let dir = TempDir::new().expect("temp dir");

    // Longer than `sockaddr_un.sun_path`, so ssh cannot bind it and exits at
    // once with `ExitOnForwardFailure=yes`.
    let unbindable = dir.path().join(format!("{}.sock", "l".repeat(120)));
    let error = HostForward::establish(
        &host,
        "/tmp/whatever.sock",
        LocalEndpoint::Socket(unbindable),
    )
    .expect_err("ssh cannot bind a path that long");

    let message = format!("{error:#}");
    assert!(
        message.contains("ssh forward to localhost"),
        "unhelpful error: {message}"
    );
}

/// A missing binary is an *outcome*, not an error: it is the one failure the
/// caller can fix, by deploying one and asking again.
#[test]
fn ensure_remote_daemon_reports_a_missing_binary_as_an_outcome() {
    let Some(host) = loopback() else { return };

    let outcome = ensure_remote_daemon(
        &host,
        "/nonexistent/ade-daemon",
        "/tmp/nonexistent.sock",
        "/tmp/nonexistent-state",
    )
    .expect("a missing binary is not an error");

    assert_eq!(outcome, EnsureOutcome::NotInstalled);
}

/// Start-if-absent over ssh: the first call brings a daemon up, the second
/// finds it, and both report a version.
#[test]
fn ensure_remote_daemon_starts_one_and_then_finds_it() {
    let Some(host) = loopback() else { return };
    let Some(binary) = daemon_binary() else {
        return;
    };
    let dir = TempDir::new().expect("temp dir");
    let socket = dir.path().join("daemon.sock").display().to_string();
    let state = dir.path().join("state").display().to_string();

    let started = ensure_remote_daemon(&host, &binary, &socket, &state).expect("starting a daemon");
    let found = ensure_remote_daemon(&host, &binary, &socket, &state).expect("finding it again");

    // Before the assertions, so a failing one cannot leak the daemon: it is in
    // its own session on purpose, and the pid file is the only handle on it.
    // Killing it over the same ssh host is fitting.
    if let Ok(pid) = fs::read_to_string(PathBuf::from(&state).join("daemon.pid")) {
        let _ = host.run(&argv(["kill", "-9", pid.trim()]));
    }

    let EnsureOutcome::Listening(started) = started else {
        panic!("the daemon binary is right there: {started:?}");
    };
    let EnsureOutcome::Listening(found) = found else {
        panic!("the daemon it just started is still there: {found:?}");
    };
    assert!(started.starts_with("ade-daemon "), "got {started:?}");
    assert_eq!(started, found);
    assert!(
        Version::parse(&found).is_some(),
        "the version line parses: {found:?}"
    );
}
