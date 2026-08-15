//! Getting the daemon binary onto a host, without ever disturbing one that is
//! already running.
//!
//! The shape mirrors Zed's own remote-server flow — probe the version, upload
//! the platform binary, start it detached — with one rule bolted on top that
//! Zed does not need:
//!
//! > **A daemon holding live sessions is never restarted or replaced.**
//!
//! Restarting the daemon kills every PTY it owns, which is precisely the thing
//! the daemon exists to prevent. So version skew is *not* resolved by replacing
//! the binary. It is resolved by not caring: [`crate::proto`] is plain serde
//! JSON with the default unknown-field behaviour, new fields are added as
//! `Option`/`#[serde(default)]`, and a newer client therefore talks to an older
//! daemon by simply not using what it does not have. Protocol evolution is
//! additive; binary upgrades are rare, deliberate, and only ever happen when
//! nothing is running.
//!
//! Concretely, [`ensure_daemon`] writes the binary in exactly two cases:
//! nothing is installed ([`DeployOutcome::Installed`]), or an older binary is
//! installed *and* no daemon socket exists ([`DeployOutcome::Replaced`]). Every
//! other case leaves the host alone and says so.
//!
//! Everything runs through [`HostExec`], so the logic is transport-free and
//! testable against [`LocalHost`] — which is not only the test double but the
//! real local-deployment path.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context as _, Result, bail};

use crate::process::QuietCommand as _;

/// Where the daemon binary lives on a host.
pub const DEFAULT_BIN_PATH: &str = "~/.ade/bin/ade-daemon";

/// Where the daemon listens.
///
/// Pinned under `~/.ade` rather than left to the daemon's own
/// `$XDG_RUNTIME_DIR` default: over ssh the client has to name the same path
/// the daemon will bind, and `$XDG_RUNTIME_DIR` is set for interactive logins
/// but frequently not for `ssh host command`. One fixed path means both sides
/// agree on every host.
pub const DEFAULT_SOCKET_PATH: &str = "~/.ade/daemon.sock";

/// Where the daemon keeps `sessions.json` and its log.
pub const DEFAULT_STATE_DIR: &str = "~/.ade/daemon";

/// Mode the uploaded binary must end up with.
pub const BINARY_MODE: u32 = 0o755;

/// What a command did on the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecOutput {
    /// `-1` when the process was killed by a signal rather than exiting.
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// The two things deployment needs from a host, local or remote.
///
/// Both are blocking on purpose: deployment is a rare, deliberate, strictly
/// sequential operation, and making it async would buy nothing but colour.
///
/// `argv` is executed **without a shell**: no globbing, no `~`, no `$VAR`.
/// [`ensure_daemon`] resolves `~` itself and passes absolute paths, and asks
/// for a shell explicitly (`sh -c …`) on the one occasion it needs one.
///
/// # Implementing this over ssh
///
/// A future `SshHost` must pass, on *every* invocation and explicitly rather
/// than relying on the user's config:
///
/// - `-o BatchMode=yes` — a password prompt must fail fast instead of wedging
///   the channel; ADE assumes key/agent auth and never implements its own.
/// - `-o ControlMaster=no -o ControlPath=none`, **from a Windows client only** —
///   not an optimisation left unused but a hard requirement there: Windows
///   OpenSSH has no Unix-socket multiplexing, `ControlPath` fails with
///   `getsockname failed: Not a socket`, and `ssh.exe` aborts outright if the
///   user's `Host *` stanza turns it on. Passing these overrides the user's
///   config, which is why a macOS or Linux client does not — nothing there is
///   broken by multiplexing, so its `~/.ssh/config` is respected.
///
/// It must also shell-quote `argv` when joining it into the single command
/// string ssh sends, since the remote side always runs it through a login
/// shell. [`shell_quote`] is here for that.
pub trait HostExec {
    /// Run `argv` on the host and capture what it did.
    fn run(&self, argv: &[String]) -> Result<ExecOutput>;

    /// Write `bytes` to `remote_path` with mode [`BINARY_MODE`].
    fn upload(&self, bytes: &[u8], remote_path: &str) -> Result<()>;
}

/// A three-part version, compared field by field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parse `--version` output: the last whitespace-separated token, which
    /// covers both `ade_session_daemon 1.2.3` and a bare `1.2.3`.
    ///
    /// Strict on purpose — anything that is not three integers is `None`, and
    /// `None` means *do not touch the host*. Guessing at a version is how you
    /// end up overwriting a binary you did not understand.
    pub fn parse(text: &str) -> Option<Self> {
        let token = text.split_whitespace().next_back()?;
        let mut parts = token.trim_start_matches('v').split('.');
        let version = Self {
            major: parts.next()?.parse().ok()?,
            minor: parts.next()?.parse().ok()?,
            patch: parts.next()?.parse().ok()?,
        };
        parts.next().is_none().then_some(version)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// What to deploy, and where.
#[derive(Clone, Debug)]
pub struct DeployConfig {
    /// The platform binary's bytes. The caller picks the platform; this module
    /// only moves bytes.
    pub binary: Vec<u8>,
    /// The version those bytes report from `--version`.
    pub expected_version: Version,
    /// May start with `~/`; [`ensure_daemon`] expands it against the host's
    /// `$HOME`.
    pub bin_path: String,
    pub socket_path: String,
    pub state_dir: String,
}

impl DeployConfig {
    pub fn new(binary: Vec<u8>, expected_version: Version) -> Self {
        Self {
            binary,
            expected_version,
            bin_path: DEFAULT_BIN_PATH.to_owned(),
            socket_path: DEFAULT_SOCKET_PATH.to_owned(),
            state_dir: DEFAULT_STATE_DIR.to_owned(),
        }
    }

    pub fn with_bin_path(mut self, path: impl Into<String>) -> Self {
        self.bin_path = path.into();
        self
    }

    pub fn with_socket_path(mut self, path: impl Into<String>) -> Self {
        self.socket_path = path.into();
        self
    }

    pub fn with_state_dir(mut self, path: impl Into<String>) -> Self {
        self.state_dir = path.into();
        self
    }
}

/// What [`ensure_daemon`] did, and why.
///
/// Only [`Installed`](Self::Installed) and [`Replaced`](Self::Replaced) touch
/// the host. The three `Kept*` variants are the "leave it alone" outcomes and
/// exist separately so a caller can tell an ordinary no-op from a skew it might
/// want to report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeployOutcome {
    /// No binary was there; the configured bytes were written.
    Installed,
    /// The installed binary already reports `expected_version`.
    AlreadyCurrent { version: Version },
    /// An older binary was installed and no daemon socket existed, so it was
    /// replaced. Nothing was killed — there was nothing to kill.
    Replaced { previous: Version },
    /// An older binary is installed and a daemon socket exists. Left untouched:
    /// replacing it is a step towards restarting it, and a restart kills PTYs.
    /// Talking to it is fine — protocol evolution is additive.
    KeptOlder { version: Version },
    /// The host is *ahead* of this client. Never downgraded; the same additive
    /// rule makes an older client on a newer daemon safe.
    KeptNewer { version: Version },
    /// `--version` produced something unparseable. Treated as "unknown", which
    /// means untouched.
    KeptUnknown { output: String },
    /// Deployment was never attempted: the caller already knows where a
    /// runnable binary is. See [`DaemonEndpoint::preinstalled`].
    NotAttempted,
}

impl DeployOutcome {
    /// Whether the binary on the host was written by this call.
    pub fn wrote_binary(&self) -> bool {
        matches!(self, Self::Installed | Self::Replaced { .. })
    }
}

/// How to reach the daemon on a host once [`ensure_daemon`] is done.
///
/// Paths are absolute — `~` is already expanded — so they can be handed
/// straight to a proxy argv or compared against what a daemon reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonEndpoint {
    pub bin_path: String,
    pub socket_path: String,
    pub state_dir: String,
    /// The version the host reports, once known. `None` when nothing was
    /// installed yet or `--version` could not be parsed.
    pub version: Option<Version>,
    pub outcome: DeployOutcome,
}

impl DaemonEndpoint {
    /// An endpoint for a binary that is already in place, with no deployment
    /// step at all.
    ///
    /// This is the **local** host's case: the binary ADE would deploy is the
    /// one it is running beside, so uploading it to `~/.ade/bin` would only
    /// pin whichever build happened to be installed first — and a version
    /// number that does not move between builds ([`Version`] comes from the
    /// crate version) would then keep it pinned forever. Deployment exists for
    /// hosts we can only reach over ssh.
    pub fn preinstalled(
        bin_path: impl Into<String>,
        socket_path: impl Into<String>,
        state_dir: impl Into<String>,
    ) -> Self {
        Self {
            bin_path: bin_path.into(),
            socket_path: socket_path.into(),
            state_dir: state_dir.into(),
            version: None,
            outcome: DeployOutcome::NotAttempted,
        }
    }

    /// The command that turns an ssh channel into a frame stream. Feed it to
    /// [`ChildConnection::spawn`](crate::transport::ChildConnection::spawn)
    /// locally, or append it to an ssh argv for a remote host.
    pub fn proxy_argv(&self) -> Vec<String> {
        vec![
            self.bin_path.clone(),
            "--stdio-proxy".to_owned(),
            "--socket".to_owned(),
            self.socket_path.clone(),
            "--state-dir".to_owned(),
            self.state_dir.clone(),
        ]
    }
}

/// Make sure `host` has a daemon binary we are willing to talk to.
///
/// See the module docs for the policy. This never starts, stops or restarts a
/// daemon: starting one is the proxy's job, and stopping one is nobody's.
pub fn ensure_daemon(host: &dyn HostExec, config: &DeployConfig) -> Result<DaemonEndpoint> {
    let home = if needs_home(config) {
        Some(host_home(host)?)
    } else {
        None
    };
    let bin_path = expand_home(&config.bin_path, home.as_deref());
    let socket_path = expand_home(&config.socket_path, home.as_deref());
    let state_dir = expand_home(&config.state_dir, home.as_deref());

    // Through a shell, so that "there is no such file" is an exit code (127)
    // rather than a spawn error. That is what ssh reports for a missing remote
    // binary, and running the local probe the same way keeps the two hosts
    // from disagreeing about what "not installed" looks like.
    let probe = host.run(&shell(&format!("{} --version", shell_quote(&bin_path))))?;
    if !probe.success() {
        // Nothing runnable at that path: no binary, or one this host cannot
        // execute. Either way installing is safe — writing a file never
        // disturbs a process that is already running its own image.
        install(host, config, &bin_path)?;
        return Ok(DaemonEndpoint {
            bin_path,
            socket_path,
            state_dir,
            version: Some(config.expected_version),
            outcome: DeployOutcome::Installed,
        });
    }

    let Some(installed) = Version::parse(&probe.stdout) else {
        return Ok(DaemonEndpoint {
            bin_path,
            socket_path,
            state_dir,
            version: None,
            outcome: DeployOutcome::KeptUnknown {
                output: probe.stdout,
            },
        });
    };

    let outcome = match installed.cmp(&config.expected_version) {
        std::cmp::Ordering::Equal => DeployOutcome::AlreadyCurrent { version: installed },
        std::cmp::Ordering::Greater => DeployOutcome::KeptNewer { version: installed },
        std::cmp::Ordering::Less => {
            if daemon_socket_exists(host, &socket_path)? {
                DeployOutcome::KeptOlder { version: installed }
            } else {
                install(host, config, &bin_path)?;
                DeployOutcome::Replaced {
                    previous: installed,
                }
            }
        }
    };
    let version = if outcome.wrote_binary() {
        config.expected_version
    } else {
        installed
    };
    log::debug!(
        "deploy: installed {installed}, this client {} — {outcome:?}",
        config.expected_version
    );
    Ok(DaemonEndpoint {
        bin_path,
        socket_path,
        state_dir,
        version: Some(version),
        outcome,
    })
}

/// Hex sha256 of `bytes` — the identity two builds of the daemon are told
/// apart by.
///
/// The [`Version`] machinery above cannot do this job: every dev build reports
/// the same crate version, so a host that got a daemon binary once would be
/// "current" forever. The daemon hashes its own executable at startup and
/// reports it in `HelloAck::binary_hash`; a client hashes the bytes it would
/// deploy and compares. Equal means the host runs exactly this build.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Install `config.binary` over whatever is at `bin_path`, but only while no
/// daemon socket exists.
///
/// This is the second half of a client-driven upgrade: the daemon has already
/// accepted a `Shutdown` (it held only tombstones) and unlinked its socket, so
/// there is nothing to disturb. The socket check is kept anyway — a daemon
/// that raced back to life, or a `Shutdown` that never really happened, must
/// win over the upgrade, in the same "never restart a running daemon"
/// direction [`ensure_daemon`] errs.
pub fn replace_daemon(host: &dyn HostExec, config: &DeployConfig) -> Result<DaemonEndpoint> {
    let home = if needs_home(config) {
        Some(host_home(host)?)
    } else {
        None
    };
    let bin_path = expand_home(&config.bin_path, home.as_deref());
    let socket_path = expand_home(&config.socket_path, home.as_deref());
    let state_dir = expand_home(&config.state_dir, home.as_deref());

    if daemon_socket_exists(host, &socket_path)? {
        bail!(
            "a daemon socket exists at {socket_path}; refusing to replace the binary under a \
             daemon that may be running"
        );
    }
    // Only for the outcome report — hash inequality already made the decision.
    let probe = host.run(&shell(&format!("{} --version", shell_quote(&bin_path))))?;
    let previous = Version::parse(&probe.stdout).unwrap_or_default();
    install(host, config, &bin_path)?;
    Ok(DaemonEndpoint {
        bin_path,
        socket_path,
        state_dir,
        version: Some(config.expected_version),
        outcome: DeployOutcome::Replaced { previous },
    })
}

/// `mkdir -p` the parent, then write the bytes.
fn install(host: &dyn HostExec, config: &DeployConfig, bin_path: &str) -> Result<()> {
    log::debug!(
        "installing ade-daemon {} ({} bytes) at {bin_path}",
        config.expected_version,
        config.binary.len()
    );
    if let Some(parent) = Path::new(bin_path).parent().and_then(Path::to_str)
        && !parent.is_empty()
    {
        let made = host.run(&argv(["mkdir", "-p", parent]))?;
        if !made.success() {
            bail!("could not create {parent} on the host: {}", made.stderr);
        }
    }
    host.upload(&config.binary, bin_path)
}

/// Is there a daemon socket at `socket_path`?
///
/// Deliberately a file-type test rather than a connect: a *stale* socket file
/// reads as "running" and keeps the old binary, which is the harmless
/// direction to be wrong in. The expensive direction — deciding nothing is
/// running when something is — is the one that costs somebody their PTYs.
fn daemon_socket_exists(host: &dyn HostExec, socket_path: &str) -> Result<bool> {
    let command = format!("test -S {}", shell_quote(socket_path));
    Ok(host.run(&shell(&command))?.success())
}

fn needs_home(config: &DeployConfig) -> bool {
    [&config.bin_path, &config.socket_path, &config.state_dir]
        .into_iter()
        .any(|path| path.starts_with('~'))
}

/// The host's `$HOME`, asked for once per [`ensure_daemon`] and only when some
/// configured path actually starts with `~`.
fn host_home(host: &dyn HostExec) -> Result<String> {
    let output = host.run(&shell("printf %s \"$HOME\""))?;
    if !output.success() || output.stdout.trim().is_empty() {
        bail!("could not read $HOME on the host: {}", output.stderr);
    }
    Ok(output.stdout.trim().to_owned())
}

fn expand_home(path: &str, home: Option<&str>) -> String {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => format!("{}/{rest}", home.trim_end_matches('/')),
        _ => path.to_owned(),
    }
}

fn argv<'a>(parts: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    parts.into_iter().map(str::to_owned).collect()
}

/// An argv that runs `command` through a POSIX shell.
fn shell(command: &str) -> Vec<String> {
    argv(["sh", "-c", command])
}

/// Wrap `text` in single quotes so a POSIX shell sees it literally.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Runs commands and writes files on this machine.
///
/// Both the test double for [`HostExec`] and the real deployment path for a
/// local host — there is no separate "local" code path to drift out of sync.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalHost;

impl HostExec for LocalHost {
    #[allow(
        clippy::disallowed_methods,
        reason = "HostExec::run is a blocking contract by design; deployment is \
        a rare sequential operation and the async spawn buys nothing here"
    )]
    fn run(&self, argv: &[String]) -> Result<ExecOutput> {
        let Some((program, arguments)) = argv.split_first() else {
            bail!("an empty argv cannot be run");
        };
        let output = std::process::Command::new(program)
            .args(arguments)
            .quiet()
            .output()
            .with_context(|| format!("running {program}"))?;
        Ok(ExecOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Write via a sibling temp file and rename.
    ///
    /// Not just for atomicity: truncating an executable that is currently
    /// running fails with `ETXTBSY` on Linux, while a rename leaves the running
    /// process on its old inode and puts the new bytes in place regardless.
    fn upload(&self, bytes: &[u8], remote_path: &str) -> Result<()> {
        static NEXT_UPLOAD: AtomicU64 = AtomicU64::new(1);

        let path = Path::new(remote_path);
        let mut temp = path.as_os_str().to_owned();
        temp.push(format!(
            ".ade-upload-{}-{}",
            std::process::id(),
            NEXT_UPLOAD.fetch_add(1, Ordering::Relaxed)
        ));
        let temp = PathBuf::from(temp);
        std::fs::write(&temp, bytes).with_context(|| format!("writing {}", temp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(BINARY_MODE))
                .with_context(|| format!("setting mode on {}", temp.display()))?;
        }
        std::fs::rename(&temp, path)
            .with_context(|| format!("renaming {} to {}", temp.display(), path.display()))?;
        Ok(())
    }
}
