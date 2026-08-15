//! Deployment against a local host. No ssh, no network, no daemon — the
//! "binary" is a shell script that prints a version, because the only thing
//! [`ensure_daemon`] knows about a binary is its bytes and what `--version`
//! says.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use ade_session::deploy::{
    BINARY_MODE, DeployConfig, DeployOutcome, HostExec, LocalHost, Version, ensure_daemon,
    replace_daemon, sha256_hex,
};
use tempfile::TempDir;

const OLDER: Version = Version::new(0, 1, 0);
const CURRENT: Version = Version::new(0, 2, 0);
const NEWER: Version = Version::new(0, 3, 0);

/// A stand-in for the daemon binary: it answers `--version` and nothing else.
fn binary_reporting(version: &str) -> Vec<u8> {
    format!("#!/bin/sh\nprintf 'ade_session_daemon {version}\\n'\n").into_bytes()
}

/// Bytes that must never reach the host in the "leave it alone" cases: if they
/// do, the file content assertion says exactly which test's policy broke.
fn must_not_be_written() -> Vec<u8> {
    b"#!/bin/sh\nprintf 'this should not have been installed\\n'\n".to_vec()
}

fn config(dir: &TempDir, binary: Vec<u8>, expected: Version) -> DeployConfig {
    DeployConfig::new(binary, expected)
        .with_bin_path(
            dir.path()
                .join("bin")
                .join("ade-daemon")
                .display()
                .to_string(),
        )
        .with_socket_path(dir.path().join("daemon.sock").display().to_string())
        .with_state_dir(dir.path().join("state").display().to_string())
}

/// Put an executable at `path`, creating its directory.
fn preinstall(path: &str, bytes: &[u8]) {
    let path = Path::new(path);
    fs::create_dir_all(path.parent().expect("a parented path")).expect("creating the bin dir");
    fs::write(path, bytes).expect("writing the binary");
    fs::set_permissions(path, fs::Permissions::from_mode(BINARY_MODE)).expect("chmod");
}

fn mode_of(path: &str) -> u32 {
    fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

#[test]
fn a_missing_binary_is_installed() {
    let dir = TempDir::new().expect("temp dir");
    let config = config(&dir, binary_reporting("0.2.0"), CURRENT);

    let endpoint = ensure_daemon(&LocalHost, &config).expect("deploying");

    assert_eq!(endpoint.outcome, DeployOutcome::Installed);
    assert_eq!(endpoint.version, Some(CURRENT));
    assert_eq!(
        fs::read(&endpoint.bin_path).expect("reading"),
        config.binary
    );
    assert_eq!(mode_of(&endpoint.bin_path), BINARY_MODE);
    assert_eq!(
        endpoint.proxy_argv(),
        vec![
            endpoint.bin_path.clone(),
            "--stdio-proxy".to_owned(),
            "--socket".to_owned(),
            endpoint.socket_path.clone(),
            "--state-dir".to_owned(),
            endpoint.state_dir,
        ]
    );
}

#[test]
fn a_current_binary_is_left_byte_for_byte_alone() {
    let dir = TempDir::new().expect("temp dir");
    let config = config(&dir, must_not_be_written(), CURRENT);
    let installed = binary_reporting("0.2.0");
    preinstall(&config.bin_path, &installed);

    let endpoint = ensure_daemon(&LocalHost, &config).expect("deploying");

    assert_eq!(
        endpoint.outcome,
        DeployOutcome::AlreadyCurrent { version: CURRENT }
    );
    assert!(!endpoint.outcome.wrote_binary());
    assert_eq!(fs::read(&config.bin_path).expect("reading"), installed);
}

/// The load-bearing case: an older binary with a daemon socket next to it is
/// left exactly where it is, because replacing it is the first step towards
/// restarting it and a restart kills PTYs.
#[test]
fn an_older_binary_with_a_daemon_socket_is_kept() {
    let dir = TempDir::new().expect("temp dir");
    let config = config(&dir, must_not_be_written(), CURRENT);
    let installed = binary_reporting("0.1.0");
    preinstall(&config.bin_path, &installed);
    let _listener =
        std::os::unix::net::UnixListener::bind(&config.socket_path).expect("binding a socket");

    let endpoint = ensure_daemon(&LocalHost, &config).expect("deploying");

    assert_eq!(
        endpoint.outcome,
        DeployOutcome::KeptOlder { version: OLDER }
    );
    assert_eq!(endpoint.version, Some(OLDER));
    assert_eq!(fs::read(&config.bin_path).expect("reading"), installed);
}

/// The one upgrade path: older binary, no socket, so nothing can be holding a
/// session and the bytes may be swapped.
#[test]
fn an_older_binary_with_no_daemon_is_replaced() {
    let dir = TempDir::new().expect("temp dir");
    let config = config(&dir, binary_reporting("0.2.0"), CURRENT);
    preinstall(&config.bin_path, &binary_reporting("0.1.0"));

    let endpoint = ensure_daemon(&LocalHost, &config).expect("deploying");

    assert_eq!(
        endpoint.outcome,
        DeployOutcome::Replaced { previous: OLDER }
    );
    assert_eq!(endpoint.version, Some(CURRENT));
    assert_eq!(fs::read(&config.bin_path).expect("reading"), config.binary);
    assert_eq!(mode_of(&config.bin_path), BINARY_MODE);
}

/// A host ahead of this client is never downgraded; the additive protocol rule
/// cuts both ways.
#[test]
fn a_newer_binary_is_never_downgraded() {
    let dir = TempDir::new().expect("temp dir");
    let config = config(&dir, must_not_be_written(), CURRENT);
    let installed = binary_reporting("0.3.0");
    preinstall(&config.bin_path, &installed);

    let endpoint = ensure_daemon(&LocalHost, &config).expect("deploying");

    assert_eq!(
        endpoint.outcome,
        DeployOutcome::KeptNewer { version: NEWER }
    );
    assert_eq!(fs::read(&config.bin_path).expect("reading"), installed);
}

/// Unparseable `--version` output means "unknown", and unknown means untouched
/// — there is no socket here at all, so only the parse failure can be what
/// stops the replacement.
#[test]
fn an_unparseable_version_is_surfaced_and_changes_nothing() {
    let dir = TempDir::new().expect("temp dir");
    let config = config(&dir, must_not_be_written(), CURRENT);
    let installed = binary_reporting("built-from-source");
    preinstall(&config.bin_path, &installed);

    let endpoint = ensure_daemon(&LocalHost, &config).expect("deploying");

    match &endpoint.outcome {
        DeployOutcome::KeptUnknown { output } => {
            assert!(output.contains("built-from-source"), "got {output:?}")
        }
        other => panic!("expected KeptUnknown, got {other:?}"),
    }
    assert_eq!(endpoint.version, None);
    assert_eq!(fs::read(&config.bin_path).expect("reading"), installed);
}

/// The hash-identity upgrade path: same crate version, different bytes, no
/// socket — [`ensure_daemon`] would call it current, [`replace_daemon`] swaps
/// it because hash inequality was already decided elsewhere.
#[test]
fn replace_swaps_a_same_version_binary_when_no_daemon_runs() {
    let dir = TempDir::new().expect("temp dir");
    let config = config(&dir, binary_reporting("0.2.0"), CURRENT);
    preinstall(
        &config.bin_path,
        b"#!/bin/sh\nprintf 'ade_session_daemon 0.2.0 but different bytes\\n'\n",
    );

    let endpoint = replace_daemon(&LocalHost, &config).expect("replacing");

    assert!(endpoint.outcome.wrote_binary());
    assert_eq!(fs::read(&config.bin_path).expect("reading"), config.binary);
    assert_eq!(mode_of(&config.bin_path), BINARY_MODE);
}

#[test]
fn concurrent_uploads_do_not_share_a_temp_file() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("ade-daemon");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let payloads: Vec<_> = (0..8).map(|byte| vec![byte; 2 * 1024 * 1024]).collect();
    let threads: Vec<_> = payloads
        .iter()
        .cloned()
        .map(|payload| {
            let barrier = barrier.clone();
            let path = path.clone();
            std::thread::spawn(move || {
                barrier.wait();
                LocalHost.upload(&payload, &path.display().to_string())
            })
        })
        .collect();

    for thread in threads {
        thread.join().expect("upload thread").expect("upload");
    }
    let installed = fs::read(path).expect("installed bytes");
    assert!(
        payloads.contains(&installed),
        "the installed binary must be one complete upload"
    );
}

/// The safety rail survives even in the forced path: a socket means a daemon
/// may be running, and nothing is written under a running daemon.
#[test]
fn replace_refuses_while_a_daemon_socket_exists() {
    let dir = TempDir::new().expect("temp dir");
    let config = config(&dir, must_not_be_written(), CURRENT);
    let installed = binary_reporting("0.2.0");
    preinstall(&config.bin_path, &installed);
    let _listener =
        std::os::unix::net::UnixListener::bind(&config.socket_path).expect("binding a socket");

    let error = replace_daemon(&LocalHost, &config).expect_err("must refuse");

    assert!(
        error.to_string().contains("refusing to replace"),
        "got {error:#}"
    );
    assert_eq!(fs::read(&config.bin_path).expect("reading"), installed);
}

/// Pin the hash format both ends of the wire compare: lowercase hex sha256.
#[test]
fn the_binary_identity_hash_is_lowercase_hex_sha256() {
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn versions_parse_strictly_or_not_at_all() {
    assert_eq!(
        Version::parse("ade_session_daemon 1.2.3"),
        Some(Version::new(1, 2, 3))
    );
    assert_eq!(Version::parse("1.2.3\n"), Some(Version::new(1, 2, 3)));
    assert_eq!(Version::parse("v1.2.3"), Some(Version::new(1, 2, 3)));
    assert_eq!(Version::parse("1.2"), None);
    assert_eq!(Version::parse("1.2.3.4"), None);
    assert_eq!(Version::parse("1.2.3-rc1"), None);
    assert_eq!(Version::parse(""), None);
    assert!(Version::new(0, 2, 0) > Version::new(0, 1, 9));
    assert!(Version::new(1, 0, 0) > Version::new(0, 9, 9));
}
