//! Reaching a host over ssh: [`SshHost`] for one-shot commands, [`HostForward`]
//! for the long-lived connection everything else rides on.
//!
//! # One ssh connection per host
//!
//! ADE never opens an ssh connection per operation, per terminal or per
//! session. A remote host is reached by forwarding its daemon socket to a local
//! one:
//!
//! ```text
//! ssh -N -o ExitOnForwardFailure=yes -L <local.sock>:<remote.sock> <host>
//! ```
//!
//! OpenSSH carries every connection made to `<local.sock>` as its own *channel*
//! over that single ssh connection, so an attach client, a control connection
//! and a status subscription are three channels and still one process. The
//! client then talks to `<local.sock>` exactly as it talks to a local daemon
//! socket — same [`Connection`](crate::Connection), same frames, no proxy
//! process in the middle.
//!
//! The *local* end of that forward is a [`LocalEndpoint`], and it is a Unix
//! socket only where the ssh client can bind one. Windows OpenSSH cannot, so it
//! forwards to a loopback TCP port instead — the far end stays a Unix socket
//! either way, because that end belongs to the remote sshd.
//!
//! The one thing that cannot ride the forward is starting the daemon that the
//! forward points at, because there is nothing listening yet. That is a single
//! short-lived `ssh <host> <bin> --ensure` before the forward is established;
//! see [`ensure_remote_daemon`].
//!
//! # Flags that are not optional
//!
//! Every invocation here passes `-o BatchMode=yes` explicitly, for the reasons
//! written down on [`HostExec`] — it overrides the user's config rather than
//! merely not relying on it.
//!
//! The `-o ControlMaster=no -o ControlPath=none` pair rides along **on Windows
//! clients only**. It is a Windows-OpenSSH requirement, not an optimisation, so
//! forcing it on a macOS or Linux client would override a working user config
//! for nothing. The client platform is the build platform, so the choice is
//! made at compile time; see [`SSH_BASE_FLAGS`].
//!
//! Every `ssh` here is also spawned [`quiet`](crate::process::QuietCommand),
//! so `ssh.exe` gets no console window of its own. The forward is the one that
//! matters most: it is long-lived, so an unflagged spawn would pin a console
//! window open for the whole session rather than merely flashing one.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use crate::deploy::{BINARY_MODE, ExecOutput, HostExec, shell_quote};
use crate::process::QuietCommand as _;

/// Flags passed on *every* ssh invocation, before the user's own arguments.
///
/// See the module docs and [`HostExec`]: `BatchMode` makes a password prompt
/// fail fast instead of wedging the channel, and the `ControlMaster` pair
/// overrides a user config that would otherwise break the Windows client.
///
/// The pair is therefore emitted on Windows builds only. The client platform is
/// the build platform, and only a Windows *client* is broken by multiplexing —
/// a macOS or Linux client's `~/.ssh/config` is left to mean what it says.
#[cfg(windows)]
pub const SSH_BASE_FLAGS: &[&str] = &[
    "-o",
    "BatchMode=yes",
    "-o",
    "ControlMaster=no",
    "-o",
    "ControlPath=none",
];

#[cfg(not(windows))]
pub const SSH_BASE_FLAGS: &[&str] = &["-o", "BatchMode=yes"];

/// A host reachable over OpenSSH.
///
/// `destination` is whatever `ssh` itself accepts — `host`, `user@host`, or an
/// alias out of the user's `~/.ssh/config`, which is deliberately still
/// consulted: ADE never implements its own auth or host database.
///
/// `extra_args` are inserted after [`SSH_BASE_FLAGS`] and before the
/// destination, e.g. `["-i", "/path/key", "-o", "IdentitiesOnly=yes", "-p",
/// "2222"]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SshHost {
    pub destination: String,
    pub extra_args: Vec<String>,
}

impl SshHost {
    pub fn new(destination: impl Into<String>) -> Self {
        Self {
            destination: destination.into(),
            extra_args: Vec::new(),
        }
    }

    pub fn with_extra_args<I>(mut self, args: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        self.extra_args = args.into_iter().map(Into::into).collect();
        self
    }

    /// The full argv for running `argv` on this host.
    ///
    /// Each element is shell-quoted because ssh joins everything after the
    /// destination with spaces and hands the result to the remote login shell;
    /// without quoting, an argument containing a space would arrive as two.
    pub fn run_argv(&self, argv: &[String]) -> Vec<String> {
        let mut out = self.ssh_argv();
        out.push("--".to_owned());
        out.extend(argv.iter().map(|argument| shell_quote(argument)));
        out
    }

    /// `ssh` plus flags plus the destination, with no command yet.
    fn ssh_argv(&self) -> Vec<String> {
        let mut out = vec!["ssh".to_owned()];
        out.extend(SSH_BASE_FLAGS.iter().map(|flag| (*flag).to_owned()));
        out.extend(self.extra_args.iter().cloned());
        out.push(self.destination.clone());
        out
    }
}

impl HostExec for SshHost {
    #[allow(
        clippy::disallowed_methods,
        reason = "HostExec::run is a blocking contract by design; see the trait docs"
    )]
    fn run(&self, argv: &[String]) -> Result<ExecOutput> {
        if argv.is_empty() {
            bail!("an empty argv cannot be run");
        }
        let ssh = self.run_argv(argv);
        let output = std::process::Command::new(&ssh[0])
            .args(&ssh[1..])
            .quiet()
            .output()
            .with_context(|| format!("running ssh {}", self.destination))?;
        Ok(ExecOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Pipe `bytes` into a remote `cat`, then rename into place.
    ///
    /// Same tmp-then-rename dance as
    /// [`LocalHost::upload`](crate::deploy::LocalHost::upload), and for the same
    /// two reasons: the destination never exists half-written, and truncating a
    /// *running* executable fails with `ETXTBSY` while renaming over it does
    /// not.
    #[allow(
        clippy::disallowed_methods,
        reason = "upload is the blocking half of the same contract, and it has \
        to configure stdin to stream the bytes"
    )]
    fn upload(&self, bytes: &[u8], remote_path: &str) -> Result<()> {
        use std::io::Write as _;

        let script = upload_script(remote_path);

        log::debug!(
            "uploading {} bytes to {remote_path} on {}",
            bytes.len(),
            self.destination
        );
        let ssh = self.run_argv(&["sh".to_owned(), "-c".to_owned(), script]);
        let mut child = std::process::Command::new(&ssh[0])
            .args(&ssh[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .quiet()
            .spawn()
            .with_context(|| format!("running ssh {}", self.destination))?;
        {
            let mut stdin = child.stdin.take().context("ssh has no stdin pipe")?;
            stdin
                .write_all(bytes)
                .with_context(|| format!("writing {remote_path} to {}", self.destination))?;
        }
        let output = child.wait_with_output().context("waiting for ssh")?;
        if !output.status.success() {
            bail!(
                "could not write {remote_path} on {}: {}",
                self.destination,
                String::from_utf8_lossy(&output.stderr).trim(),
            );
        }
        Ok(())
    }
}

fn upload_script(remote_path: &str) -> String {
    let mut script = String::new();
    if let Some(parent) = Path::new(remote_path).parent().and_then(Path::to_str)
        && !parent.is_empty()
    {
        script.push_str(&format!("mkdir -p {} && ", shell_quote(parent)));
    }
    script.push_str(&format!(
        "temp=$(mktemp {template}) && trap 'rm -f \"$temp\"' EXIT HUP INT TERM && \
         cat > \"$temp\" && chmod {mode:o} \"$temp\" && mv \"$temp\" {path}",
        template = shell_quote(&format!("{remote_path}.ade-upload.XXXXXX")),
        mode = BINARY_MODE,
        path = shell_quote(remote_path),
    ));
    script
}

/// Exit status a POSIX shell reports for "command not found", which is what a
/// remote shell says about a daemon binary that is not installed.
pub const EXIT_NOT_FOUND: i32 = 127;

/// What [`ensure_remote_daemon`] found.
///
/// "Not installed" is an *outcome* and not an error, because it is the one
/// failure the caller can fix: it deploys the binary
/// ([`crate::source`] + [`crate::deploy`]) and asks again. Every other way
/// `--ensure` can fail is still an `Err`, so a caller that cannot deploy needs
/// no `if` to keep behaving as before.
///
/// Kept a typed value rather than a message to match on: an anyhow chain is
/// prose, and prose is not a control-flow signal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// A daemon is listening. Carries the version line it printed.
    Listening(String),
    /// Nothing runnable is at `bin_path` — the remote shell answered
    /// [`EXIT_NOT_FOUND`]. Nothing was started, and nothing was installed:
    /// this function never writes to a host.
    NotInstalled,
}

/// Make sure the host's daemon is listening, without disturbing one that is.
///
/// Runs `<bin_path> --ensure --socket … --state-dir …`, which connects to the
/// socket and only starts a daemon if nothing answers. This is the one
/// operation that cannot ride the forward, since the forward's far end is the
/// socket this call brings into existence.
pub fn ensure_remote_daemon(
    host: &SshHost,
    bin_path: &str,
    socket: &str,
    state_dir: &str,
) -> Result<EnsureOutcome> {
    log::debug!("ensuring ade-daemon at {bin_path} on {}", host.destination);
    let output = host.run(&[
        bin_path.to_owned(),
        "--ensure".to_owned(),
        "--socket".to_owned(),
        socket.to_owned(),
        "--state-dir".to_owned(),
        state_dir.to_owned(),
    ])?;
    if output.exit_code == EXIT_NOT_FOUND {
        log::debug!(
            "no ade-daemon at {bin_path} on {}; it can be deployed",
            host.destination
        );
        return Ok(EnsureOutcome::NotInstalled);
    }
    if !output.success() {
        bail!(
            "ade-daemon --ensure failed on {}: {}",
            host.destination,
            output.stderr.trim(),
        );
    }
    Ok(EnsureOutcome::Listening(output.stdout.trim().to_owned()))
}

/// The loopback address a TCP-mode forward binds its local end on.
///
/// Spelled as a literal address and never as `localhost`: on a dual-stack
/// machine `localhost` can resolve to `::1` first, and ssh would then bind an
/// address the client is not dialling.
pub const LOOPBACK_ADDRESS: &str = "127.0.0.1";

/// The local end of a [`HostForward`] — what a client on *this* machine
/// connects to in order to reach the host's daemon socket.
///
/// [`Self::Socket`] is the default wherever it works, because the forward then
/// inherits the filesystem's permissions: a process that cannot read the
/// socket's directory cannot reach the host at all.
///
/// [`Self::Loopback`] exists for Windows. Binding a local Unix socket is a
/// *client-side* capability that Windows OpenSSH does not have, so `-L
/// <path>:<remote.sock>` fails there — but `-L 127.0.0.1:<port>:<remote.sock>`
/// works, since the AF_UNIX end of that forward is the remote sshd's business
/// and never the client's. The price is that a loopback port is reachable by
/// every process on the machine; it is accepted only because the alternative on
/// Windows is no remote hosts at all.
///
/// Both variants exist on every platform so that callers can name the one they
/// want without `cfg`; only the operations that std itself gates are gated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalEndpoint {
    Socket(PathBuf),
    Loopback(u16),
}

impl LocalEndpoint {
    /// Reserve a free loopback port for a TCP-mode forward.
    ///
    /// Binding a listener, reading the port it was given and dropping it is
    /// racy by construction: another process can take the port between the drop
    /// and ssh's own bind. That is accepted — it is the portable way to be
    /// *given* a free port, the window is microseconds long, and
    /// `ExitOnForwardFailure=yes` turns a lost race into an immediate error
    /// carrying ssh's own words rather than a forward pointing somewhere else.
    pub fn loopback() -> Result<Self> {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .context("reserving a loopback port for the ssh forward")?;
        let port = listener
            .local_addr()
            .context("reading the reserved loopback port")?
            .port();
        drop(listener);
        Ok(Self::Loopback(port))
    }

    /// The `-L` argument: what ssh binds on this side, and the socket it
    /// carries every channel to on the far side.
    pub fn forward_spec(&self, remote_socket: &str) -> String {
        match self {
            Self::Socket(path) => format!("{}:{remote_socket}", path.display()),
            Self::Loopback(port) => format!("{LOOPBACK_ADDRESS}:{port}:{remote_socket}"),
        }
    }

    /// Make this end bindable. Only a socket has anything to do: its directory
    /// has to exist and be private, and ssh refuses to bind over a path that is
    /// already there, so a stale one is removed first.
    fn prepare(&self) -> Result<()> {
        match self {
            #[cfg(unix)]
            Self::Socket(path) => {
                use std::os::unix::fs::PermissionsExt as _;

                if let Some(parent) = path.parent()
                    && !parent.as_os_str().is_empty()
                {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                        .with_context(|| format!("restricting {}", parent.display()))?;
                }
                match std::fs::remove_file(path) {
                    Ok(()) => Ok(()),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(err) => Err(err)
                        .with_context(|| format!("removing the stale socket {}", path.display())),
                }
            }
            #[cfg(not(unix))]
            Self::Socket(path) => bail!(
                "this platform's ssh client cannot bind the local Unix socket {}; \
                 forward to a loopback port instead",
                path.display()
            ),
            // Nothing to reserve: the port was reserved when this value was
            // made, and ssh binds it itself.
            Self::Loopback(_) => Ok(()),
        }
    }

    /// Does anything accept a connection here yet? ssh binds the local end at
    /// startup, so this is the readiness signal.
    fn accepts_connections(&self) -> bool {
        match self {
            #[cfg(unix)]
            Self::Socket(path) => std::os::unix::net::UnixStream::connect(path).is_ok(),
            #[cfg(not(unix))]
            Self::Socket(_) => false,
            Self::Loopback(port) => {
                std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, *port)).is_ok()
            }
        }
    }

    /// Undo what binding left behind. ssh unlinks its own socket on a clean
    /// exit but not after a kill; a port needs nothing.
    fn cleanup(&self) {
        match self {
            Self::Socket(path) => {
                let _ = std::fs::remove_file(path);
            }
            Self::Loopback(_) => {}
        }
    }
}

impl std::fmt::Display for LocalEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Socket(path) => write!(formatter, "{}", path.display()),
            Self::Loopback(port) => write!(formatter, "{LOOPBACK_ADDRESS}:{port}"),
        }
    }
}

/// The persistent ssh connection to a host: one process, one forwarded socket,
/// every channel.
#[derive(Debug)]
pub struct HostForward {
    child: std::process::Child,
    /// The job object the ssh process is assigned to. Holding it is what bounds
    /// the forward's life to this process's own: the OS closes the handle
    /// whenever we go away, including a crash that never runs [`Drop`], and
    /// closing it kills ssh and everything ssh started.
    #[cfg(windows)]
    _guard: util::process::ProcessTreeGuard,
    /// Where the daemon can now be reached on this machine. Connect to it
    /// exactly as to a local daemon socket.
    local: LocalEndpoint,
    destination: String,
    remote_socket: String,
}

impl HostForward {
    /// Total time [`establish`](Self::establish) waits for the forward to start
    /// accepting connections.
    pub const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    /// First retry delay; it doubles up to [`MAX_RETRY_DELAY`](Self::MAX_RETRY_DELAY).
    const FIRST_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);
    const MAX_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

    /// The argv that carries the forward.
    ///
    /// `-N` because no remote command is wanted — the channels are the point —
    /// and `ExitOnForwardFailure=yes` so a forward that cannot be set up kills
    /// the connection instead of leaving a live ssh with nothing behind the
    /// socket.
    pub fn argv(host: &SshHost, remote_socket: &str, local: &LocalEndpoint) -> Vec<String> {
        let mut argv = vec![
            "ssh".to_owned(),
            "-N".to_owned(),
            "-o".to_owned(),
            "ExitOnForwardFailure=yes".to_owned(),
        ];
        argv.extend(SSH_BASE_FLAGS.iter().map(|flag| (*flag).to_owned()));
        argv.extend(host.extra_args.iter().cloned());
        argv.push("-L".to_owned());
        argv.push(local.forward_spec(remote_socket));
        argv.push(host.destination.clone());
        argv
    }

    /// Spawn the forward and wait until `local` accepts a connection.
    ///
    /// ssh binds the local end itself, at startup, so "accepts a connection" is
    /// the readiness signal — whether the daemon behind it answers is a later
    /// question, and the handshake is where it gets asked.
    ///
    /// Two consequences worth knowing, and they hold for a loopback port
    /// exactly as they do for a socket:
    ///
    /// - The probe **opens a channel** and drops it, so the far-side daemon
    ///   sees one connection that says nothing and goes away. That is a
    ///   disconnect like any other, and disconnects never cost a session.
    /// - `ExitOnForwardFailure=yes` covers the *local* bind, which is all ssh
    ///   can check up front: the remote socket is only contacted when a channel
    ///   is opened. A forward whose far end does not exist therefore
    ///   establishes fine and fails one connection at a time, with an immediate
    ///   EOF. Callers must run `--ensure` before trusting the tunnel; the
    ///   handshake is what turns a missing daemon into a legible error.
    #[allow(
        clippy::disallowed_methods,
        reason = "the forward is a long-lived child whose stdio must be \
        configured; establishing it is a rare, sequential operation"
    )]
    pub fn establish(host: &SshHost, remote_socket: &str, local: LocalEndpoint) -> Result<Self> {
        local.prepare()?;

        let argv = Self::argv(host, remote_socket, &local);
        let child = Self::command(&argv)
            .spawn()
            .with_context(|| format!("spawning the ssh forward to {}", host.destination))?;

        #[cfg(windows)]
        let guard = match Self::guard_process_tree(child.id()) {
            Ok(guard) => guard,
            Err(error) => {
                // No `HostForward` owns this child yet, so `Drop` cannot reap
                // it and this is the one path that has to do it by hand. It is
                // reaped rather than kept, because an unguarded forward is
                // exactly the child that outlives us: it would hold the local
                // end bound with nobody left to kill it.
                Self::kill_and_wait(child, &host.destination);
                local.cleanup();
                let destination = &host.destination;
                return Err(error.context(format!("guarding the ssh forward to {destination}")));
            }
        };

        let mut forward = Self {
            child,
            #[cfg(windows)]
            _guard: guard,
            local,
            destination: host.destination.clone(),
            remote_socket: remote_socket.to_owned(),
        };
        forward.wait_until_ready()?;
        Self::drain_stderr(&mut forward.child)?;
        Ok(forward)
    }

    /// Everything about the forward's child except spawning it: the argv from
    /// [`argv`](Self::argv), the stdio the readiness loop and the error
    /// messages depend on, and — on Windows — a current directory of its own.
    #[allow(
        clippy::disallowed_methods,
        reason = "the forward is a long-lived child whose stdio must be configured"
    )]
    fn command(argv: &[String]) -> std::process::Command {
        let mut command = std::process::Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            // Captured rather than inherited so that a BatchMode failure
            // ("Permission denied", "Host key verification failed") comes back
            // verbatim in the error instead of scrolling past on a log.
            .stderr(std::process::Stdio::piped())
            // This child outlives every operation on the host, so on Windows
            // the missing flag would not flash a console window but keep one
            // on screen for as long as the host is reachable.
            .quiet();

        // A child inherits our current directory, and on Windows it holds that
        // directory open: nothing can delete or rename it while the child runs.
        // This child runs for as long as the host is reachable, so left alone
        // it would pin whatever directory Zed was started from — a checkout or
        // a worktree the user then cannot remove. Unix has no such lock, and
        // the forward's directory there stays what it always was.
        #[cfg(windows)]
        command.current_dir(util::process::stable_child_dir());

        command
    }

    /// Put the spawned ssh process — and everything it spawns afterwards — in a
    /// job object that dies with us.
    #[cfg(windows)]
    fn guard_process_tree(pid: u32) -> Result<util::process::ProcessTreeGuard> {
        let guard = util::process::ProcessTreeGuard::new()?;
        guard.assign_process(pid)?;
        Ok(guard)
    }

    /// End a child that no `HostForward` owns. Failures are logged and not
    /// propagated: the caller is already returning the error that got here, and
    /// a child that cannot be killed is not a fact the caller can act on.
    #[cfg(windows)]
    fn kill_and_wait(mut child: std::process::Child, destination: &str) {
        if let Err(error) = child.kill() {
            log::warn!("could not kill the ssh forward to {destination}: {error}");
        }
        if let Err(error) = child.wait() {
            log::warn!("could not reap the ssh forward to {destination}: {error}");
        }
    }

    fn wait_until_ready(&mut self) -> Result<()> {
        let deadline = std::time::Instant::now() + Self::READY_TIMEOUT;
        let mut delay = Self::FIRST_RETRY_DELAY;
        loop {
            // Liveness *before* the probe, deliberately. The probe cannot tell
            // who is listening, and a lost race for the local end — a port
            // another process bound first, a socket path somebody else owns —
            // leaves our ssh dead behind a working answer. An exited child is
            // never ready, whoever replies.
            if let Some(status) = self.child.try_wait().context("checking on ssh")? {
                let stderr = self.take_stderr();
                bail!(
                    "the ssh forward to {} exited ({status}) before {} was usable: {stderr}",
                    self.destination,
                    self.remote_socket,
                );
            }
            if self.local.accepts_connections() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                let stderr = self.take_stderr();
                bail!(
                    "the ssh forward to {} did not make {} usable within {:?}: {stderr}",
                    self.destination,
                    self.local,
                    Self::READY_TIMEOUT,
                );
            }
            std::thread::sleep(delay);
            delay = (delay * 2).min(Self::MAX_RETRY_DELAY);
        }
    }

    /// Whatever ssh has written to stderr, for an error message. Only called
    /// once the child is gone, so the pipe is at EOF and this cannot block.
    fn take_stderr(&mut self) -> String {
        use std::io::Read as _;

        let Some(mut stderr) = self.child.stderr.take() else {
            return String::new();
        };
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text.trim().to_owned()
    }

    fn drain_stderr(child: &mut std::process::Child) -> Result<()> {
        let mut stderr = child
            .stderr
            .take()
            .context("ssh forward stderr capture failed")?;
        std::thread::Builder::new()
            .name("ade-ssh-stderr".to_owned())
            .spawn(move || {
                if let Err(error) = std::io::copy(&mut stderr, &mut std::io::sink()) {
                    log::debug!("stopped draining ssh forward stderr: {error}");
                }
            })
            .context("starting the ssh forward stderr drain")?;
        Ok(())
    }

    /// Is the ssh process still running? A dead forward means every channel on
    /// it is gone and the caller has to re-[`establish`](Self::establish).
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Where the daemon is reachable on this machine.
    pub fn local(&self) -> &LocalEndpoint {
        &self.local
    }

    pub fn destination(&self) -> &str {
        &self.destination
    }
}

/// Dropping the forward ends the ssh connection and every channel on it.
///
/// That is a *detach* and nothing more: the remote daemon is in its own
/// session, ignores SIGHUP, and keeps every PTY it owns.
impl Drop for HostForward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.local.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> SshHost {
        SshHost::new("user@example.com").with_extra_args([
            "-i",
            "/keys/id",
            "-o",
            "IdentitiesOnly=yes",
            "-p",
            "2222",
        ])
    }

    fn index_of(argv: &[String], needle: &str) -> usize {
        argv.iter()
            .position(|argument| argument == needle)
            .unwrap_or_else(|| panic!("{needle:?} is missing from {argv:?}"))
    }

    fn base_flags() -> Vec<String> {
        SSH_BASE_FLAGS
            .iter()
            .map(|flag| (*flag).to_owned())
            .collect()
    }

    /// The `ControlMaster` pair overrides the user's ssh config, so it is forced
    /// from Windows clients — which multiplexing breaks — and from nowhere else.
    fn assert_control_master_override(argv: &[String]) {
        assert_eq!(
            argv.windows(2)
                .any(|pair| pair == ["-o", "ControlMaster=no"]),
            cfg!(windows),
            "ControlMaster=no belongs to Windows clients only"
        );
        assert_eq!(
            argv.windows(2)
                .any(|pair| pair == ["-o", "ControlPath=none"]),
            cfg!(windows),
            "ControlPath=none belongs to Windows clients only"
        );
    }

    #[test]
    fn shell_quoting_survives_everything_a_shell_would_eat() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("'"), r"''\'''");
        assert_eq!(shell_quote("a'b'c"), r"'a'\''b'\''c'");
        assert_eq!(shell_quote("$HOME `id` \\"), "'$HOME `id` \\'");
        assert_eq!(shell_quote("ünïcøde ✓"), "'ünïcøde ✓'");
    }

    #[test]
    fn concurrent_uploads_get_distinct_remote_temp_files() {
        let script = upload_script("/home/user name/.ade/bin/ade-daemon");
        assert!(script.contains("mktemp '/home/user name/.ade/bin/ade-daemon.ade-upload.XXXXXX'"));
        assert!(script.contains("cat > \"$temp\""));
        assert!(script.contains("mv \"$temp\" '/home/user name/.ade/bin/ade-daemon'"));
        assert!(script.contains("trap 'rm -f \"$temp\"'"));
    }

    /// `BatchMode` is unconditional; the `ControlMaster` pair is the Windows
    /// client's alone, and a Unix client's `~/.ssh/config` is left to stand.
    #[test]
    fn only_a_windows_client_forces_the_control_master_overrides() {
        assert!(
            SSH_BASE_FLAGS
                .windows(2)
                .any(|pair| pair == ["-o", "BatchMode=yes"]),
            "BatchMode is not platform-dependent"
        );
        assert_eq!(SSH_BASE_FLAGS.contains(&"ControlMaster=no"), cfg!(windows));
        assert_eq!(SSH_BASE_FLAGS.contains(&"ControlPath=none"), cfg!(windows));
        assert_eq!(SSH_BASE_FLAGS.len(), if cfg!(windows) { 6 } else { 2 });
    }

    /// The flags that are not optional are on every invocation, and the user's
    /// own arguments come after them (so a `-o` here overrides nothing that
    /// matters) and before the destination.
    #[test]
    fn run_argv_carries_the_mandatory_flags_then_extras_then_the_destination() {
        let host = host();
        let argv = host.run_argv(&["echo".to_owned(), "a b".to_owned()]);
        let base = base_flags();

        assert_eq!(argv[0], "ssh");
        assert_eq!(argv[1..1 + base.len()], base[..]);
        assert!(argv.windows(2).any(|pair| pair == ["-o", "BatchMode=yes"]));
        assert_control_master_override(&argv);

        let extras = index_of(&argv, "-i");
        let destination = index_of(&argv, "user@example.com");
        let separator = index_of(&argv, "--");
        assert!(extras < destination, "extra args precede the destination");
        assert!(destination < separator, "the destination precedes `--`");
        assert_eq!(&argv[extras..destination], &host.extra_args[..]);
        assert_eq!(&argv[separator + 1..], ["'echo'", "'a b'"]);
    }

    #[test]
    fn run_argv_quotes_every_element_for_the_remote_shell() {
        let argv = SshHost::new("host").run_argv(&[
            "printf".to_owned(),
            "%s\n".to_owned(),
            "it's a file".to_owned(),
        ]);
        let separator = index_of(&argv, "--");
        assert_eq!(
            &argv[separator + 1..],
            ["'printf'", "'%s\n'", r"'it'\''s a file'"]
        );
    }

    #[test]
    fn forward_argv_is_a_command_less_streamlocal_forward() {
        let host = host();
        let argv = HostForward::argv(
            &host,
            "/remote/daemon.sock",
            &LocalEndpoint::Socket(PathBuf::from("/local/daemon.sock")),
        );

        assert_eq!(argv[0], "ssh");
        assert_eq!(argv[1], "-N");
        assert!(
            argv.windows(2)
                .any(|pair| pair == ["-o", "ExitOnForwardFailure=yes"])
        );
        assert!(argv.windows(2).any(|pair| pair == ["-o", "BatchMode=yes"]));
        assert_control_master_override(&argv);

        let forward = index_of(&argv, "-L");
        assert_eq!(argv[forward + 1], "/local/daemon.sock:/remote/daemon.sock");
        assert_eq!(argv.last().expect("a destination"), &host.destination);
        assert!(
            index_of(&argv, "-i") < forward,
            "extra args precede the forward spec"
        );
        assert!(
            !argv.iter().any(|argument| argument == "--"),
            "there is no remote command to separate"
        );
    }

    /// The Windows client's forward: a loopback port on this side, the same
    /// Unix socket on the far side — which is the whole point, since the far
    /// end is bound by the remote sshd and not by the client.
    #[test]
    fn a_loopback_local_end_forwards_a_port_to_the_same_remote_socket() {
        let host = host();
        let argv = HostForward::argv(
            &host,
            "/remote/daemon.sock",
            &LocalEndpoint::Loopback(54321),
        );

        let forward = index_of(&argv, "-L");
        assert_eq!(argv[forward + 1], "127.0.0.1:54321:/remote/daemon.sock");
        assert_eq!(argv[1], "-N");
        assert!(
            argv.windows(2)
                .any(|pair| pair == ["-o", "ExitOnForwardFailure=yes"])
        );
        let base = base_flags();
        assert_eq!(argv[4..4 + base.len()], base[..]);
        assert_control_master_override(&argv);
        assert_eq!(argv.last().expect("a destination"), &host.destination);
    }

    /// A reserved port is a real, free one, and it is named the same way in the
    /// forward spec and in anything printed at the user.
    #[test]
    fn a_reserved_loopback_port_is_free_and_reads_as_an_address() {
        let endpoint = LocalEndpoint::loopback().expect("reserving a port");
        let LocalEndpoint::Loopback(port) = endpoint else {
            panic!("expected a loopback endpoint, got {endpoint:?}");
        };
        assert_ne!(port, 0, "the port is the one the kernel handed out");
        assert_eq!(endpoint.to_string(), format!("127.0.0.1:{port}"));
        assert_eq!(
            endpoint.forward_spec("/remote/daemon.sock"),
            format!("127.0.0.1:{port}:/remote/daemon.sock")
        );
        // Nothing holds it: the listener was dropped so ssh can bind it.
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .expect("the reserved port was released");

        // And two reservations do not collide.
        assert_ne!(endpoint, LocalEndpoint::loopback().expect("a second port"));
    }

    /// Guarding the child changed how it is spawned, so what is spawned is
    /// pinned here: the command runs [`HostForward::argv`] and nothing else.
    #[test]
    fn the_forward_command_runs_exactly_the_forward_argv() {
        let argv = HostForward::argv(
            &host(),
            "/remote/daemon.sock",
            &LocalEndpoint::Loopback(54321),
        );
        let command = HostForward::command(&argv);

        assert_eq!(command.get_program().to_string_lossy(), argv[0]);
        let arguments: Vec<String> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(arguments, argv[1..].to_vec());
    }

    #[cfg(unix)]
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "the test exercises the blocking HostForward process directly"
    )]
    fn draining_stderr_prevents_forward_backpressure() {
        let argv = ["sh", "-c", "dd if=/dev/zero bs=1048576 count=1 >&2"]
            .map(str::to_owned)
            .to_vec();
        let mut child = HostForward::command(&argv)
            .spawn()
            .expect("spawning stand-in forward");
        HostForward::drain_stderr(&mut child).expect("starting stderr drain");

        let status = child.wait().expect("waiting for stand-in forward");
        assert!(status.success());
    }

    /// The Windows client gives the forward a directory of its own, because a
    /// child there holds its current directory open for as long as it runs and
    /// this one runs for as long as the host is reachable. That directory is
    /// outside the checkout, which is the property that makes it safe; on every
    /// other platform the child's directory is untouched.
    #[test]
    fn the_forward_child_gets_a_stable_directory_on_windows_only() {
        let argv = HostForward::argv(
            &host(),
            "/remote/daemon.sock",
            &LocalEndpoint::Loopback(54321),
        );
        let command = HostForward::command(&argv);

        #[cfg(windows)]
        {
            let directory = command.get_current_dir().expect("a current directory");
            assert_eq!(directory, util::process::stable_child_dir());
            assert!(directory.is_dir(), "{directory:?} is not a directory");

            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(Path::parent)
                .expect("this crate sits two directories below the repository");
            assert!(
                !directory.starts_with(repository),
                "{directory:?} is inside {repository:?}, so the forward would pin it"
            );
        }

        #[cfg(not(windows))]
        assert_eq!(command.get_current_dir(), None, "the child is not moved");
    }

    /// The lifetime seam itself, with a stand-in for ssh: a child spawned the
    /// way the forward spawns one dies when the guard that
    /// [`HostForward::establish`] keeps is dropped — including when nothing
    /// runs `Drop` on the forward, since the OS closes the handle either way.
    #[cfg(windows)]
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "the test spawns the same kind of long-lived child `establish` does"
    )]
    fn dropping_the_guard_ends_the_child_the_forward_spawned() {
        let argv = ["ping", "-n", "60", "127.0.0.1"]
            .map(str::to_owned)
            .to_vec();
        let mut child = HostForward::command(&argv).spawn().expect("spawning ping");
        let guard = HostForward::guard_process_tree(child.id()).expect("guarding ping");
        assert!(
            child.try_wait().expect("checking on ping").is_none(),
            "ping should outlive the guard's creation"
        );

        drop(guard);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while child.try_wait().expect("checking on ping").is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "ping outlived the guard"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// A forward's job object must cover descendants too. This stand-in keeps
    /// the descendant creation behind a gate so assignment happens before the
    /// process tree exists, then verifies that dropping the guard ends both
    /// processes without involving an SSH server.
    #[cfg(windows)]
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "the test spawns a deterministic PowerShell process tree"
    )]
    fn dropping_the_forward_guard_ends_a_descendant_tree() {
        use std::time::{Duration, Instant};

        fn process_is_alive(pid: u32) -> bool {
            let Ok(output) = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
                .output()
            else {
                return false;
            };
            String::from_utf8_lossy(&output.stdout).contains(&format!("\",\"{pid}\","))
        }

        let temp_dir = tempfile::tempdir().expect("creating test directory");
        let pid_file = temp_dir.path().join("ping.pid");
        let start_file = temp_dir.path().join("start");
        let command = format!(
            "$deadline = (Get-Date).AddSeconds(30); while (!(Test-Path -LiteralPath '{}')) {{ if ((Get-Date) -gt $deadline) {{ exit 1 }}; Start-Sleep -Milliseconds 10 }}; $p = Start-Process -FilePath ping.exe -ArgumentList @('-n','60','127.0.0.1') -PassThru -WindowStyle Hidden; Set-Content -LiteralPath '{}' -Value $p.Id; Wait-Process -Id $p.Id",
            start_file.display(),
            pid_file.display()
        );
        let argv = ["powershell.exe", "-NoProfile", "-Command", &command]
            .map(str::to_owned)
            .to_vec();
        let mut child = HostForward::command(&argv)
            .spawn()
            .expect("spawning stand-in");
        let root_pid = child.id();
        let guard = HostForward::guard_process_tree(root_pid).expect("guarding stand-in");
        std::fs::write(&start_file, b"start").expect("releasing stand-in");

        let deadline = Instant::now() + Duration::from_secs(30);
        let grandchild_pid = loop {
            if let Ok(contents) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = contents.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for descendant"
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        assert!(child.try_wait().expect("checking stand-in").is_none());

        drop(guard);
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().expect("checking stand-in").is_none() {
            assert!(Instant::now() < deadline, "root outlived forward guard");
            std::thread::sleep(Duration::from_millis(25));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_is_alive(grandchild_pid) {
            assert!(
                Instant::now() < deadline,
                "descendant outlived forward guard"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn a_socket_local_end_reads_as_its_path() {
        let endpoint = LocalEndpoint::Socket(PathBuf::from("/local/daemon.sock"));
        assert_eq!(endpoint.to_string(), "/local/daemon.sock");
        assert_eq!(
            endpoint.forward_spec("/remote/daemon.sock"),
            "/local/daemon.sock:/remote/daemon.sock"
        );
    }
}
