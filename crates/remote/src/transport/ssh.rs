use crate::{
    RemoteArch, RemoteClientDelegate, RemoteOs, RemotePlatform,
    remote_client::{CommandTemplate, Interactive, RemoteConnection, RemoteConnectionOptions},
    transport::{parse_platform, parse_shell},
};
use anyhow::{Context as _, Result, anyhow};
use async_trait::async_trait;
use collections::HashMap;
use futures::{
    AsyncReadExt as _, FutureExt as _,
    channel::mpsc::{Sender, UnboundedReceiver, UnboundedSender},
    select_biased,
};
use gpui::{App, AppContext as _, AsyncApp, Task};
use parking_lot::Mutex;
use paths::remote_server_dir_relative;
use release_channel::{AppCommitSha, AppVersion, ReleaseChannel};
use rpc::proto::Envelope;
use semver::Version;
pub use settings::SshPortForwardOption;
#[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
use sha2::Digest as _;
use smol::fs;
use std::{
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tempfile::TempDir;
use util::command::{Child, Stdio};
use util::{
    paths::{PathStyle, RemotePathBuf},
    rel_path::RelPath,
    shell::ShellKind,
};

/// How long to wait for SSH to connect when no askpass prompt has opened.
const SSH_CONNECTION_PROMPT_TIMEOUT: Duration = Duration::from_secs(17);

#[cfg(not(windows))]
const CONTROL_MASTER_CHECK_TIMEOUT: Duration = Duration::from_secs(1);

fn upload_status(size: u64, elapsed: Duration) -> String {
    format!(
        "Uploading remote development server ({} MiB, {}s)",
        size.div_ceil(1024 * 1024),
        elapsed.as_secs()
    )
}

/// How the remote server binary got to the host, for the connect timing log.
enum ServerBinaryOutcome {
    /// The host already had a binary under the wanted name.
    Reused,
    /// This connect put it there (built from source, or downloaded).
    Uploaded,
}

/// The source identity of a dev build.
struct DevBuildId {
    /// The version segment of the server binary name: `build-<sha>`, or
    /// `build-<sha>-dirty` when the worktree does not match the sha.
    version: String,
    /// True when tracked files match the sha, i.e. when the name is an honest
    /// description of the bytes. Only then may a binary already on the host
    /// under that name be reused instead of rebuilt.
    clean: bool,
}

impl DevBuildId {
    fn version_for_artifact(&self, artifact_hash: Option<&str>) -> String {
        if self.clean {
            self.version.clone()
        } else {
            artifact_hash.map_or_else(
                || self.version.clone(),
                |artifact_hash| format!("{}-{artifact_hash}", self.version),
            )
        }
    }
}

/// `None` when the build had no git to ask (empty sha), which keeps the old
/// un-versioned `zed-remote-server-dev-build` name and its always-rebuild
/// behaviour.
fn dev_build_id(commit: Option<&AppCommitSha>) -> Option<DevBuildId> {
    let commit = commit?;
    let sha: String = commit.full().chars().take(12).collect();
    let dirty = commit.source_is_dirty();
    Some(DevBuildId {
        version: if dirty {
            format!("build-{sha}-dirty")
        } else {
            format!("build-{sha}")
        },
        clean: !dirty,
    })
}

fn server_binary_name(
    release_channel: ReleaseChannel,
    version: &Version,
    dev_build_id: Option<&DevBuildId>,
    artifact_hash: Option<&str>,
    is_windows: bool,
) -> String {
    let version = match release_channel {
        ReleaseChannel::Dev => dev_build_id.map_or_else(
            || "build".to_string(),
            |id| id.version_for_artifact(artifact_hash),
        ),
        _ => version.to_string(),
    };
    format!(
        "zed-remote-server-{}-{}{}",
        release_channel.dev_name(),
        version,
        if is_windows { ".exe" } else { "" }
    )
}

#[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
async fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .await
        .with_context(|| format!("opening remote server binary at {}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let bytes_read = file
            .read(&mut buffer)
            .await
            .with_context(|| format!("hashing remote server binary at {}", path.display()))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
fn remove_stale_dev_server_binaries_script(dir: &str, keep_path: &str) -> String {
    format!(
        "for file in {dir}/zed-remote-server-dev-build*; do [ \"$file\" = {keep_path} ] || rm -f \"$file\"; done"
    )
}

pub(crate) struct SshRemoteConnection {
    socket: SshSocket,
    master_process: Mutex<Option<MasterProcess>>,
    /// Whether `kill()` has been called. Separate from `master_process` because
    /// reused ControlMaster sessions start with `master_process` as `None`.
    killed: AtomicBool,
    remote_binary_path: Option<Arc<RelPath>>,
    ssh_platform: RemotePlatform,
    ssh_os_version: Option<String>,
    ssh_path_style: PathStyle,
    ssh_shell: String,
    ssh_shell_kind: ShellKind,
    ssh_default_system_shell: String,
    _temp_dir: TempDir,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SshConnectionHost {
    IpAddr(IpAddr),
    Hostname(String),
}

impl SshConnectionHost {
    pub fn to_bracketed_string(&self) -> String {
        match self {
            Self::IpAddr(IpAddr::V4(ip)) => ip.to_string(),
            Self::IpAddr(IpAddr::V6(ip)) => format!("[{}]", ip),
            Self::Hostname(hostname) => hostname.clone(),
        }
    }

    pub fn to_string(&self) -> String {
        match self {
            Self::IpAddr(ip) => ip.to_string(),
            Self::Hostname(hostname) => hostname.clone(),
        }
    }
}

impl From<&str> for SshConnectionHost {
    fn from(value: &str) -> Self {
        if let Ok(address) = value.parse() {
            Self::IpAddr(address)
        } else {
            Self::Hostname(value.to_string())
        }
    }
}

impl From<String> for SshConnectionHost {
    fn from(value: String) -> Self {
        if let Ok(address) = value.parse() {
            Self::IpAddr(address)
        } else {
            Self::Hostname(value)
        }
    }
}

impl Default for SshConnectionHost {
    fn default() -> Self {
        Self::Hostname(Default::default())
    }
}

fn bracket_ipv6(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{}]", host)
    } else {
        host.to_string()
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SshConnectionOptions {
    pub host: SshConnectionHost,
    pub username: Option<String>,
    pub port: Option<u16>,
    pub password: Option<String>,
    pub args: Option<Vec<String>>,
    pub port_forwards: Option<Vec<SshPortForwardOption>>,
    pub connection_timeout: Option<u16>,

    pub nickname: Option<String>,
    pub upload_binary_over_ssh: bool,
}

impl From<settings::SshConnection> for SshConnectionOptions {
    fn from(val: settings::SshConnection) -> Self {
        SshConnectionOptions {
            host: val.host.to_string().into(),
            username: val.username,
            port: val.port,
            password: None,
            args: Some(val.args),
            nickname: val.nickname,
            upload_binary_over_ssh: val.upload_binary_over_ssh.unwrap_or_default(),
            port_forwards: val.port_forwards,
            connection_timeout: val.connection_timeout,
        }
    }
}

struct SshSocket {
    connection_options: SshConnectionOptions,
    #[cfg(not(windows))]
    socket_path: std::path::PathBuf,
    /// Extra environment variables needed for the ssh process
    envs: HashMap<String, String>,
    #[cfg(windows)]
    _proxy: askpass::PasswordProxy,
}

/// The directory to give a long-lived ssh child on Windows, or `None` to leave
/// it in Zed's own.
///
/// A child inherits Zed's current directory, and on Windows it holds that
/// directory open: nothing can delete or rename a directory a running process
/// sits in. A child that lives for as long as the connection does therefore
/// pins whatever directory Zed was started from — for a development build, the
/// checkout or worktree the user then cannot remove. Moving it to
/// [`util::process::stable_child_dir`] is what unpins it.
///
/// **But moving a child changes what its relative paths mean.** ssh resolves
/// `-i key`, `-F config`, `-o IdentityFile=key` and the program in a
/// `ProxyCommand` against the process's current directory, so a child moved out
/// from under a relative option reads a different file — or no file — and the
/// connection fails to authenticate. A pinned directory is a nuisance; a
/// connection that will not open is a broken feature, so an argument list that
/// could name a relative path keeps Zed's directory and says so in the log.
///
/// Every ssh child of one connection must agree on this, master and proxy
/// alike: two children resolving `-i key` against two different directories is
/// a connection that authenticates and then cannot start its server.
///
/// Out of reach: a relative path written in the user's own `ssh_config`. Only
/// the arguments Zed passes are visible here.
#[cfg(windows)]
fn long_lived_child_dir(args: &[String]) -> Option<PathBuf> {
    if let Some(argument) = ssh_arg_naming_a_relative_path(args) {
        log::debug!(
            "keeping the ssh child in Zed's directory: {argument:?} may name a path relative to it"
        );
        return None;
    }
    Some(util::process::stable_child_dir())
}

/// The first argument ssh could resolve against the current directory, if there
/// is one.
///
/// Deliberately generous: a false yes only keeps a directory pinned, while a
/// false no breaks the connection.
#[cfg(windows)]
fn ssh_arg_naming_a_relative_path(args: &[String]) -> Option<&str> {
    let mut args = args.iter();
    while let Some(argument) = args.next() {
        // `-i key`, `-F config`: the value is the next argument, and is judged
        // there rather than on its own turn, where a bare `key` would look like
        // a host name.
        if matches!(argument.as_str(), "-i" | "-F" | "-S" | "-E") {
            match args.next() {
                Some(value) if is_relative_value(value) => return Some(value),
                // A flag with nothing after it names no file at all.
                Some(_) | None => continue,
            }
        }
        if may_name_a_relative_path(argument) {
            return Some(argument);
        }
    }
    None
}

/// ssh options that take the path of a file on *this* machine.
#[cfg(windows)]
const SSH_PATH_OPTIONS: &[&str] = &[
    "certificatefile",
    "controlpath",
    "globalknownhostsfile",
    "identityagent",
    "identityfile",
    "pkcs11provider",
    "revokedhostkeys",
    "securitykeyprovider",
    "userknownhostsfile",
    "xauthlocation",
];

/// ssh options whose value is a command line rather than a path. The program in
/// one can itself be relative, and a command line cannot be split reliably
/// enough to tell, so any of these is treated as naming a relative path.
#[cfg(windows)]
const SSH_COMMAND_OPTIONS: &[&str] = &["knownhostscommand", "localcommand", "proxycommand"];

/// Whether ssh could resolve something in this one argument against the current
/// directory. The two-argument spellings are [`ssh_arg_naming_a_relative_path`]'s
/// to answer; this one sees an argument alone.
#[cfg(windows)]
fn may_name_a_relative_path(argument: &str) -> bool {
    // The attached spellings: `-ikey`, `-Fconfig`.
    for flag in ["-i", "-F", "-S", "-E"] {
        if let Some(value) = argument.strip_prefix(flag) {
            return is_relative_value(value);
        }
    }
    let option = argument
        .strip_prefix("-o")
        .map(|rest| rest.trim_start())
        .unwrap_or(argument);
    // `-o Name=value` and `-o "Name value"` are the same option to ssh.
    if let Some((name, value)) = option.split_once(['=', ' ']) {
        let name = name.trim().to_ascii_lowercase();
        if SSH_COMMAND_OPTIONS.contains(&name.as_str()) {
            return true;
        }
        if SSH_PATH_OPTIONS.contains(&name.as_str()) {
            return is_relative_value(value);
        }
        // An option this list has not heard of, whose value still looks like a
        // relative path, is treated as one. An unrecognized name is not the end
        // of it, though: the argument itself can be the path.
        if value.contains(['/', '\\']) && is_relative_value(value) {
            return true;
        }
    }
    // A bare argument that looks like a relative path — a `ProxyCommand`
    // continuation, a value passed apart from its `-o` — counts too.
    !argument.starts_with('-') && argument.contains(['/', '\\']) && is_relative_value(argument)
}

/// Whether ssh would resolve this value against the current directory, rather
/// than reading it as an absolute path or a home-relative one.
#[cfg(windows)]
fn is_relative_value(value: &str) -> bool {
    let value = value.trim_matches('"');
    if value.is_empty() || value.starts_with('~') {
        return false;
    }
    !std::path::Path::new(value).is_absolute()
}

/// Put a spawned ssh process — and everything it spawns afterwards — in a job
/// object that dies with Zed.
///
/// `pid` must be the pid of a child the caller still holds open; see
/// [`util::process::ProcessTreeGuard::assign_process`].
#[cfg(windows)]
fn guard_process_tree(pid: u32) -> Result<util::process::ProcessTreeGuard> {
    let guard = util::process::ProcessTreeGuard::new()?;
    guard.assign_process(pid)?;
    Ok(guard)
}

/// End an ssh process that nothing owns yet. Failures are logged and not
/// propagated: the caller is already returning the error that got here, and a
/// child that cannot be killed is not a fact the caller can act on.
#[cfg(windows)]
async fn kill_and_reap(process: &mut Child, destination: &str) {
    if let Err(error) = process.kill() {
        log::warn!("could not kill the ssh connection to {destination}: {error}");
    }
    if let Err(error) = process.status().await {
        log::warn!("could not reap the ssh connection to {destination}: {error}");
    }
}

struct MasterProcess {
    process: Child,
    _stderr_task: Option<Task<()>>,
    /// The job object the master ssh process is assigned to. Holding it for as
    /// long as the `MasterProcess` lives is what bounds ssh's life to Zed's:
    /// the OS closes the handle whenever Zed goes away, including a crash that
    /// never runs [`Drop`], and closing it ends ssh and every process ssh
    /// started.
    #[cfg(windows)]
    _guard: util::process::ProcessTreeGuard,
}

#[cfg(not(windows))]
impl MasterProcess {
    fn command(
        askpass_script_path: &std::ffi::OsStr,
        additional_args: Vec<String>,
        socket_path: &std::path::Path,
        destination: &str,
    ) -> util::command::Command {
        let mut command = util::command::new_command("ssh");
        command
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("SSH_ASKPASS", askpass_script_path)
            .args(additional_args)
            .args([
                "-N",
                "-o",
                "ControlPersist=no",
                "-o",
                "ControlMaster=yes",
                "-o",
            ])
            .arg(format!("ControlPath={}", socket_path.display()))
            .arg(destination);
        command
    }

    pub fn new(
        askpass_script_path: &std::ffi::OsStr,
        additional_args: Vec<String>,
        socket_path: &std::path::Path,
        destination: &str,
    ) -> Result<Self> {
        let process = Self::command(
            askpass_script_path,
            additional_args,
            socket_path,
            destination,
        )
        .spawn()?;

        Ok(MasterProcess {
            process,
            _stderr_task: None,
        })
    }

    pub async fn wait_connected(&mut self) -> Result<()> {
        let Some(mut stdout) = self.process.stdout.take() else {
            anyhow::bail!("ssh process stdout capture failed");
        };

        let mut output = Vec::new();
        stdout.read_to_end(&mut output).await?;
        Ok(())
    }
}

#[cfg(windows)]
impl MasterProcess {
    const CONNECTION_ESTABLISHED_MAGIC: &str = "ZED_SSH_CONNECTION_ESTABLISHED";

    pub async fn new(
        askpass_script_path: &std::ffi::OsStr,
        askpass_socket_path: &std::ffi::OsStr,
        additional_args: Vec<String>,
        destination: &str,
    ) -> Result<Self> {
        let mut process = Self::command(
            askpass_script_path,
            askpass_socket_path,
            additional_args,
            destination,
        )
        .spawn()?;

        // Assignment happens after spawn, but before the SSH handshake can
        // create the proxy descendant. `ProcessTreeGuard::assign_process`
        // documents the unavoidable post-spawn race for the general case.
        let guard = match guard_process_tree(process.id()) {
            Ok(guard) => guard,
            Err(error) => {
                // No `MasterProcess` owns this child yet, so nothing will drop
                // it and this path has to end it by hand. It is ended rather
                // than kept, because an unguarded master is exactly the child
                // that outlives Zed: ssh and everything ssh starts would keep
                // running with nobody left to kill them.
                kill_and_reap(&mut process, destination).await;
                let message = format!("guarding the ssh connection to {destination}");
                return Err(error.context(message));
            }
        };

        Ok(MasterProcess {
            process,
            _stderr_task: None,
            _guard: guard,
        })
    }

    /// Everything about the master ssh child except spawning it: the argv,
    /// the stdio that [`wait_connected`](Self::wait_connected) and the
    /// connection errors read, the askpass environment, and a current
    /// directory of its own.
    fn command(
        askpass_script_path: &std::ffi::OsStr,
        askpass_socket_path: &std::ffi::OsStr,
        additional_args: Vec<String>,
        destination: &str,
    ) -> util::command::Command {
        // On Windows, `ControlMaster` and `ControlPath` are not supported:
        // https://github.com/PowerShell/Win32-OpenSSH/issues/405
        // https://github.com/PowerShell/Win32-OpenSSH/wiki/Project-Scope
        //
        // Using an ugly workaround to detect connection establishment
        // -N doesn't work with JumpHosts as windows openssh never closes stdin in that case
        let args = [
            "-t",
            &format!("echo '{}'; exec $0", Self::CONNECTION_ESTABLISHED_MAGIC),
        ];

        let mut master_process = util::command::new_command("ssh");
        master_process
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("SSH_ASKPASS", askpass_script_path)
            .env("ZED_ASKPASS_SOCKET", askpass_socket_path)
            .args(&additional_args)
            .arg(destination)
            .args(args);

        // The master lives for as long as the connection does. See
        // [`long_lived_child_dir`] for what moving it out of Zed's directory
        // buys, and when it is not safe to.
        if let Some(directory) = long_lived_child_dir(&additional_args) {
            master_process.current_dir(directory);
        }

        master_process
    }

    pub async fn wait_connected(&mut self) -> Result<()> {
        use smol::io::AsyncBufReadExt;

        let Some(stdout) = self.process.stdout.take() else {
            anyhow::bail!("ssh process stdout capture failed");
        };

        let mut reader = smol::io::BufReader::new(stdout);

        let mut line = String::new();

        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                anyhow::bail!("ssh process exited before connection established");
            }

            if line.contains(Self::CONNECTION_ESTABLISHED_MAGIC) {
                return Ok(());
            }
        }
    }
}

impl MasterProcess {
    fn drain_stderr(&mut self, executor: &gpui::BackgroundExecutor) -> Result<()> {
        let mut stderr = self
            .process
            .stderr
            .take()
            .context("ssh process stderr capture failed")?;
        self._stderr_task = Some(executor.spawn(async move {
            let mut buffer = vec![0; 8192];
            loop {
                match stderr.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(read) => log::debug!(
                        "ssh master: {}",
                        String::from_utf8_lossy(&buffer[..read]).trim_end()
                    ),
                    Err(error) => {
                        log::debug!("stopped reading ssh master stderr: {error}");
                        break;
                    }
                }
            }
        }));
        Ok(())
    }
}

impl AsRef<Child> for MasterProcess {
    fn as_ref(&self) -> &Child {
        &self.process
    }
}

impl AsMut<Child> for MasterProcess {
    fn as_mut(&mut self) -> &mut Child {
        &mut self.process
    }
}

#[async_trait(?Send)]
impl RemoteConnection for SshRemoteConnection {
    async fn kill(&self) -> Result<()> {
        self.killed.store(true, Ordering::Release);
        let Some(mut process) = self.master_process.lock().take() else {
            log::debug!("no master process to kill (external ControlMaster session)");
            return Ok(());
        };
        process.as_mut().kill().ok();
        process.as_mut().status().await?;
        Ok(())
    }

    fn has_been_killed(&self) -> bool {
        self.killed.load(Ordering::Acquire)
    }

    async fn is_usable(&self) -> bool {
        if self.has_been_killed() {
            return false;
        }
        #[cfg(not(windows))]
        {
            control_master_is_alive(&self.socket.socket_path).await
        }
        #[cfg(windows)]
        {
            let Some(mut master_process) = self.master_process.try_lock() else {
                return false;
            };
            let Some(master_process) = master_process.as_mut() else {
                return false;
            };
            match master_process.as_mut().try_status() {
                Ok(None) => true,
                Ok(Some(_)) => false,
                Err(error) => {
                    log::debug!("failed to check SSH master process: {error}");
                    false
                }
            }
        }
    }

    fn connection_options(&self) -> RemoteConnectionOptions {
        RemoteConnectionOptions::Ssh(self.socket.connection_options.clone())
    }

    fn shell(&self) -> String {
        self.ssh_shell.clone()
    }

    fn default_system_shell(&self) -> String {
        self.ssh_default_system_shell.clone()
    }

    fn build_command(
        &self,
        input_program: Option<String>,
        input_args: &[String],
        input_env: &HashMap<String, String>,
        working_dir: Option<String>,
        port_forward: Option<(u16, String, u16)>,
        interactive: Interactive,
    ) -> Result<CommandTemplate> {
        let Self {
            ssh_path_style,
            socket,
            ssh_shell_kind,
            ssh_shell,
            ..
        } = self;
        let env = socket.envs.clone();
        let ssh_options = if port_forward.is_some() {
            socket.ssh_command_options_for_explicit_forward()
        } else {
            socket.ssh_command_options()
        };

        if self.ssh_platform.os.is_windows() {
            build_command_windows(
                input_program,
                input_args,
                input_env,
                working_dir,
                port_forward,
                env,
                *ssh_path_style,
                ssh_shell,
                *ssh_shell_kind,
                ssh_options,
                &socket.connection_options.ssh_destination(),
                interactive,
            )
        } else {
            build_command_posix(
                input_program,
                input_args,
                input_env,
                working_dir,
                port_forward,
                env,
                *ssh_path_style,
                ssh_shell,
                *ssh_shell_kind,
                ssh_options,
                &socket.connection_options.ssh_destination(),
                interactive,
            )
        }
    }

    fn build_forward_ports_command(
        &self,
        forwards: Vec<(u16, String, u16)>,
    ) -> Result<CommandTemplate> {
        let Self { socket, .. } = self;
        let mut args = socket.ssh_command_options_for_explicit_forward();
        args.push("-N".into());
        for (local_port, host, remote_port) in forwards {
            args.push("-L".into());
            args.push(format!(
                "{}:{}:{}",
                local_port,
                bracket_ipv6(&host),
                remote_port
            ));
        }
        args.push(socket.connection_options.ssh_destination());
        Ok(CommandTemplate {
            program: "ssh".into(),
            args,
            env: Default::default(),
        })
    }

    fn upload_directory(
        &self,
        src_path: PathBuf,
        dest_path: RemotePathBuf,
        cx: &App,
    ) -> Task<Result<()>> {
        let dest_path_str = dest_path.to_string();
        let src_path_display = src_path.display().to_string();

        let mut sftp_command = self.build_sftp_command();
        let mut scp_command =
            self.build_scp_command(&src_path, &dest_path_str, Some(&["-C", "-r"]));

        cx.background_spawn(async move {
            // We will try SFTP first, and if that fails, we will fall back to SCP.
            // If SCP fails also, we give up and return an error.
            // The reason we allow a fallback from SFTP to SCP is that if the user has to specify a password,
            // depending on the implementation of SSH stack, SFTP may disable interactive password prompts in batch mode.
            // This is for example the case on Windows as evidenced by this implementation snippet:
            // https://github.com/PowerShell/openssh-portable/blob/b8c08ef9da9450a94a9c5ef717d96a7bd83f3332/sshconnect2.c#L417
            if Self::is_sftp_available().await {
                log::debug!("using SFTP for directory upload");
                let mut child = sftp_command.spawn()?;
                if let Some(mut stdin) = child.stdin.take() {
                    use futures::AsyncWriteExt;
                    let sftp_batch = format!("put -r \"{src_path_display}\" \"{dest_path_str}\"\n");
                    stdin.write_all(sftp_batch.as_bytes()).await?;
                    stdin.flush().await?;
                }

                let output = child.output().await?;
                if output.status.success() {
                    return Ok(());
                }

                let stderr = String::from_utf8_lossy(&output.stderr);
                log::debug!("failed to upload directory via SFTP {src_path_display} -> {dest_path_str}: {stderr}");
            }

            log::debug!("using SCP for directory upload");
            let output = scp_command.output().await?;

            if output.status.success() {
                return Ok(());
            }

            let stderr = String::from_utf8_lossy(&output.stderr);
            log::debug!("failed to upload directory via SCP {src_path_display} -> {dest_path_str}: {stderr}");

            anyhow::bail!(
                "failed to upload directory via SFTP/SCP {} -> {}: {}",
                src_path_display,
                dest_path_str,
                stderr,
            );
        })
    }

    fn start_proxy(
        &self,
        unique_identifier: String,
        reconnect: bool,
        incoming_tx: UnboundedSender<Envelope>,
        outgoing_rx: UnboundedReceiver<Envelope>,
        connection_activity_tx: Sender<()>,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Task<Result<i32>> {
        const VARS: [&str; 3] = ["RUST_LOG", "RUST_BACKTRACE", "ZED_GENERATE_MINIDUMPS"];
        delegate.set_status(Some("Starting proxy"), cx);

        let Some(remote_binary_path) = self.remote_binary_path.clone() else {
            return Task::ready(Err(anyhow!("Remote binary path not set")));
        };

        let mut ssh_command = if self.ssh_platform.os.is_windows() {
            // TODO: Set the `VARS` environment variables, we do not have `env` on windows
            // so this needs a different approach
            let mut proxy_args = vec![];
            proxy_args.push("proxy".to_owned());
            proxy_args.push("--identifier".to_owned());
            proxy_args.push(unique_identifier);

            if reconnect {
                proxy_args.push("--reconnect".to_owned());
            }
            self.socket.ssh_command(
                self.ssh_shell_kind,
                &remote_binary_path.display(self.path_style()),
                &proxy_args,
                false,
            )
        } else {
            let mut proxy_args = vec![];
            for env_var in VARS {
                if let Some(value) = std::env::var(env_var).ok() {
                    proxy_args.push(format!("{env_var}={value}"));
                }
            }
            proxy_args.push(remote_binary_path.display(self.path_style()).into_owned());
            proxy_args.push("proxy".to_owned());
            proxy_args.push("--identifier".to_owned());
            proxy_args.push(unique_identifier);

            if reconnect {
                proxy_args.push("--reconnect".to_owned());
            }
            self.socket
                .ssh_command(self.ssh_shell_kind, "env", &proxy_args, false)
        };

        // The proxy is the second long-lived ssh child of a connection, and it
        // outlives every operation that goes through it. See
        // [`long_lived_child_dir`] for why it gets a directory of its own, and
        // when it must not: the master resolved its relative options against
        // the same directory this picks, which is what keeps them agreeing.
        #[cfg(windows)]
        if let Some(directory) =
            long_lived_child_dir(&self.socket.connection_options.additional_args())
        {
            ssh_command.current_dir(directory);
        }

        let ssh_proxy_process = match ssh_command
            // IMPORTANT: we kill this process when we drop the task that uses it.
            .kill_on_drop(true)
            .spawn()
        {
            Ok(process) => process,
            Err(error) => {
                return Task::ready(Err(
                    anyhow::Error::new(error).context("failed to spawn remote server")
                ));
            }
        };

        // `kill_on_drop` ends the direct ssh process only, and only when
        // something runs `Drop` — a crash does not. The job object is what
        // bounds the whole proxy tree to Zed's own life, since the OS closes
        // its handle however Zed goes away.
        #[cfg(windows)]
        let guard = match guard_process_tree(ssh_proxy_process.id()) {
            Ok(guard) => guard,
            Err(error) => {
                // Nothing owns this child yet — the RPC task below is what
                // would — so this path ends it by hand rather than leaving an
                // unguarded proxy running with nobody left to kill it.
                let mut ssh_proxy_process = ssh_proxy_process;
                let destination = self.socket.connection_options.ssh_destination();
                return cx.background_spawn(async move {
                    kill_and_reap(&mut ssh_proxy_process, &destination).await;
                    Err(error.context(format!(
                        "guarding the remote server proxy for {destination}"
                    )))
                });
            }
        };

        let rpc = super::handle_rpc_messages_over_child_process_stdio(
            ssh_proxy_process,
            incoming_tx,
            outgoing_rx,
            connection_activity_tx,
            cx,
        );

        // The guard has to last exactly as long as the RPC task owning the
        // proxy child: dropping it early closes the job object and kills the
        // proxy mid-session, and holding it longer would keep a dead session's
        // job open. Owning it here is what makes the two the same lifetime —
        // dropping the returned task drops both.
        #[cfg(windows)]
        let rpc = cx.background_spawn(async move {
            let _guard = guard;
            rpc.await
        });

        rpc
    }

    fn path_style(&self) -> PathStyle {
        self.ssh_path_style
    }

    fn remote_platform(&self) -> RemotePlatform {
        self.ssh_platform
    }

    fn remote_os_version(&self) -> Option<String> {
        self.ssh_os_version.clone()
    }

    fn has_wsl_interop(&self) -> bool {
        false
    }
}

/// Check if the user already has an active SSH ControlMaster session for the
/// given destination. See: https://github.com/zed-industries/zed/issues/45271
#[cfg(not(windows))]
async fn find_existing_control_master(
    destination: &str,
    additional_args: &[String],
) -> Option<PathBuf> {
    // Use `ssh -G` to resolve the user's effective SSH config for this host.
    // This expands ControlPath tokens (%h, %p, %r, %C, etc.) into actual paths.
    let output = match util::command::new_command("ssh")
        .args(additional_args)
        .arg("-G")
        .arg(destination)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
    {
        Ok(output) => output,
        Err(e) => {
            log::debug!("failed to run ssh -G: {e}");
            return None;
        }
    };

    if !output.status.success() {
        log::debug!("ssh -G failed for {destination}, skipping ControlMaster reuse");
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let control_path = stdout.lines().find_map(|line| {
        let path = line.strip_prefix("controlpath ")?.trim();
        if path == "none" || path.is_empty() {
            None
        } else {
            Some(PathBuf::from(path))
        }
    })?;

    // Verify the master is actually alive by sending a control command.
    if control_master_is_alive(&control_path).await {
        log::info!(
            "reusing existing SSH ControlMaster at {}",
            control_path.display()
        );
        Some(control_path)
    } else {
        log::debug!(
            "ControlMaster socket at {} is not alive, creating new connection",
            control_path.display()
        );
        None
    }
}

#[cfg(not(windows))]
async fn control_master_is_alive(control_path: &Path) -> bool {
    enum CheckResult {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut,
    }

    let mut process = match util::command::new_command("ssh")
        .arg("-F")
        .arg("/dev/null")
        .args(["-O", "check"])
        .arg("-o")
        .arg(format!("ControlPath={}", control_path.display()))
        .arg("control-master-check.invalid")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(process) => process,
        Err(e) => {
            log::debug!("failed to run ssh -O check: {e}");
            return false;
        }
    };

    let result = smol::future::or(
        async { CheckResult::Exited(process.status().await) },
        async {
            smol::unblock(|| std::thread::sleep(CONTROL_MASTER_CHECK_TIMEOUT)).await;
            CheckResult::TimedOut
        },
    )
    .await;

    match result {
        CheckResult::Exited(Ok(status)) => status.success(),
        CheckResult::Exited(Err(error)) => {
            log::debug!("failed to check SSH ControlMaster: {error}");
            false
        }
        CheckResult::TimedOut => {
            log::debug!(
                "SSH ControlMaster check at {} timed out",
                control_path.display()
            );
            if let Err(error) = process.kill() {
                log::debug!("failed to stop timed-out SSH ControlMaster check: {error}");
            }
            if let Err(error) = process.status().await {
                log::debug!("failed to reap timed-out SSH ControlMaster check: {error}");
            }
            false
        }
    }
}

impl SshRemoteConnection {
    pub(crate) async fn new(
        connection_options: SshConnectionOptions,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<Self> {
        use askpass::AskPassResult;

        let destination = connection_options.ssh_destination();

        let temp_dir = tempfile::Builder::new()
            .prefix("zed-ssh-session")
            .tempdir()?;

        // On non-Windows, check if the user already has an active ControlMaster
        // session for this host. If so, reuse it instead of prompting for auth.
        #[cfg(not(windows))]
        let reused_socket = if connection_options
            .port_forwards
            .as_ref()
            .is_none_or(Vec::is_empty)
            && !ssh_args_have_forwards(&connection_options.additional_args_for_scp())
        {
            find_existing_control_master(&destination, &connection_options.additional_args()).await
        } else {
            None
        };

        #[cfg(not(windows))]
        let (socket, master_process_option) = if let Some(reused_path) = reused_socket {
            delegate.set_status(Some("Connecting (reusing session)"), cx);
            log::info!("reusing existing ControlMaster, skipping authentication");
            let socket = SshSocket::new(connection_options, reused_path).await?;
            (socket, None)
        } else {
            let askpass_delegate = askpass::AskPassDelegate::new(cx, {
                let delegate = delegate.clone();
                move |prompt, tx, cx| delegate.ask_password(prompt, tx, cx)
            });

            let mut askpass =
                askpass::AskPassSession::new(cx.background_executor().clone(), askpass_delegate)
                    .await?;

            delegate.set_status(Some("Connecting"), cx);

            // Start the master SSH process, which does not do anything except
            // for establish the connection and keep it open, allowing other ssh
            // commands to reuse it via a control socket.
            let socket_path = temp_dir.path().join("ssh.sock");
            let mut master_process = MasterProcess::new(
                askpass.script_path().as_ref(),
                connection_options.additional_args(),
                &socket_path,
                &destination,
            )?;

            let result = select_biased! {
                result = askpass.run(Some(SSH_CONNECTION_PROMPT_TIMEOUT)).fuse() => {
                    match result {
                        AskPassResult::CancelledByUser => {
                            master_process.as_mut().kill().ok();
                            anyhow::bail!("SSH connection canceled")
                        }
                        AskPassResult::Timedout => {
                            anyhow::bail!("connecting to host timed out")
                        }
                    }
                }
                _ = master_process.wait_connected().fuse() => {
                    anyhow::Ok(())
                }
            };

            if let Err(e) = result {
                return Err(e.context("Failed to connect to host"));
            }

            if master_process.as_mut().try_status()?.is_some() {
                let mut output = Vec::new();
                let mut stderr = master_process
                    .as_mut()
                    .stderr
                    .take()
                    .context("ssh process stderr capture failed")?;
                stderr.read_to_end(&mut output).await?;

                let error_message = format!(
                    "failed to connect: {}",
                    String::from_utf8_lossy(&output).trim()
                );
                anyhow::bail!(error_message);
            }

            master_process.drain_stderr(&cx.background_executor())?;
            let socket = SshSocket::new(connection_options, socket_path).await?;
            drop(askpass);
            (socket, Some(master_process))
        };

        #[cfg(windows)]
        let (socket, master_process_option) = {
            let askpass_delegate = askpass::AskPassDelegate::new(cx, {
                let delegate = delegate.clone();
                move |prompt, tx, cx| delegate.ask_password(prompt, tx, cx)
            });

            let mut askpass =
                askpass::AskPassSession::new(cx.background_executor().clone(), askpass_delegate)
                    .await?;

            delegate.set_status(Some("Connecting"), cx);

            let mut master_process = MasterProcess::new(
                askpass.script_path().as_ref(),
                askpass.socket_path().as_ref(),
                connection_options.additional_args(),
                &destination,
            )
            .await?;

            let result = select_biased! {
                result = askpass.run(Some(SSH_CONNECTION_PROMPT_TIMEOUT)).fuse() => {
                    match result {
                        AskPassResult::CancelledByUser => {
                            master_process.as_mut().kill().ok();
                            anyhow::bail!("SSH connection canceled")
                        }
                        AskPassResult::Timedout => {
                            anyhow::bail!("connecting to host timed out")
                        }
                    }
                }
                _ = master_process.wait_connected().fuse() => {
                    anyhow::Ok(())
                }
            };

            if let Err(e) = result {
                return Err(e.context("Failed to connect to host"));
            }

            if master_process.as_mut().try_status()?.is_some() {
                let mut output = Vec::new();
                let mut stderr = master_process
                    .as_mut()
                    .stderr
                    .take()
                    .context("ssh process stderr capture failed")?;
                stderr.read_to_end(&mut output).await?;

                let error_message = format!(
                    "failed to connect: {}",
                    String::from_utf8_lossy(&output).trim()
                );
                anyhow::bail!(error_message);
            }

            master_process.drain_stderr(&cx.background_executor())?;
            let socket = SshSocket::new(
                connection_options,
                askpass
                    .get_password()
                    .or_else(|| askpass::EncryptedPassword::try_from("").ok())
                    .context("Failed to fetch askpass password")?,
                cx.background_executor().clone(),
            )
            .await?;
            drop(askpass);

            (socket, Some(master_process))
        };

        let is_windows = socket.probe_is_windows().await;
        log::info!("Remote is windows: {}", is_windows);

        let ssh_shell = socket.shell(is_windows).await;
        log::info!("Remote shell discovered: {}", ssh_shell);

        let ssh_shell_kind = ShellKind::new(&ssh_shell, is_windows);
        let ssh_platform = socket.platform(ssh_shell_kind, is_windows).await?;
        log::info!("Remote platform discovered: {:?}", ssh_platform);

        let ssh_os_version = socket.os_version(ssh_platform.os, ssh_shell_kind).await;
        log::info!("Remote OS version discovered: {:?}", ssh_os_version);

        let (ssh_path_style, ssh_default_system_shell) = match ssh_platform.os {
            RemoteOs::Windows => (PathStyle::Windows, ssh_shell.clone()),
            _ => (PathStyle::Unix, String::from("/bin/sh")),
        };

        let mut this = Self {
            socket,
            master_process: Mutex::new(master_process_option),
            killed: AtomicBool::new(false),
            _temp_dir: temp_dir,
            remote_binary_path: None,
            ssh_path_style,
            ssh_platform,
            ssh_os_version,
            ssh_shell,
            ssh_shell_kind,
            ssh_default_system_shell,
        };

        let (release_channel, version, commit) = cx.update(|cx| {
            (
                ReleaseChannel::global(cx),
                AppVersion::global(cx),
                AppCommitSha::try_global(cx),
            )
        });
        // A dev connect either reuses the binary already on the host or builds
        // and uploads ~460MB; those differ by about a minute, so say which one
        // happened and how long it took.
        let started_at = std::time::Instant::now();
        let (remote_binary_path, outcome) = this
            .ensure_server_binary(&delegate, release_channel, version, commit, cx)
            .await?;
        log::info!(
            "remote server ready in {:.1}s ({} {})",
            started_at.elapsed().as_secs_f32(),
            match outcome {
                ServerBinaryOutcome::Reused => "reused existing",
                ServerBinaryOutcome::Uploaded => "built and uploaded",
            },
            remote_binary_path.display(this.path_style()),
        );
        this.remote_binary_path = Some(remote_binary_path);

        Ok(this)
    }

    async fn ensure_server_binary(
        &self,
        delegate: &Arc<dyn RemoteClientDelegate>,
        release_channel: ReleaseChannel,
        version: Version,
        commit: Option<AppCommitSha>,
        cx: &mut AsyncApp,
    ) -> Result<(Arc<RelPath>, ServerBinaryOutcome)> {
        // A dev build has no version to name its server binary by, so it names
        // it by the commit it was built from: `build-<sha>`. That is what makes
        // the `binary_exists_on_server` probe below answer "does the host
        // already have THIS build" instead of "does it have some dev build",
        // which is what lets the build + upload be skipped.
        let dev_build_id = dev_build_id(commit.as_ref());
        let binary_name = server_binary_name(
            release_channel,
            &version,
            dev_build_id.as_ref(),
            None,
            self.ssh_platform.os.is_windows(),
        );
        let dst_path = paths::remote_server_dir_relative().join(
            RelPath::from_unix_str(&binary_name).context("invalid remote server binary name")?,
        );

        let binary_exists_on_server = self
            .socket
            .run_command(
                self.ssh_shell_kind,
                &dst_path.display(self.path_style()),
                &["version"],
                true,
            )
            .await
            .is_ok();

        // Reuse is only sound when the name pins the source: a clean sha means a
        // binary of that name on the host was built from exactly this commit, so
        // rebuilding it would upload the same bytes. A dirty worktree — or no sha
        // at all — cannot make that promise, and keeps the always-rebuild path.
        #[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
        let reuse_existing = matches!(release_channel, ReleaseChannel::Dev)
            && dev_build_id.as_ref().is_some_and(|id| id.clean);

        #[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
        if let Some(remote_server_path) = super::build_remote_server_from_source(
            &self.ssh_platform,
            delegate.as_ref(),
            binary_exists_on_server,
            reuse_existing,
            cx,
        )
        .await?
        {
            let dst_path = if dev_build_id.as_ref().is_some_and(|id| !id.clean) {
                let artifact_hash = cx
                    .background_spawn({
                        let remote_server_path = remote_server_path.clone();
                        async move { sha256_file(&remote_server_path).await }
                    })
                    .await?;
                let binary_name = server_binary_name(
                    release_channel,
                    &version,
                    dev_build_id.as_ref(),
                    Some(&artifact_hash),
                    self.ssh_platform.os.is_windows(),
                );
                let dst_path = paths::remote_server_dir_relative().join(
                    RelPath::from_unix_str(&binary_name)
                        .context("invalid content-addressed remote server binary name")?,
                );

                if self
                    .socket
                    .run_command(
                        self.ssh_shell_kind,
                        &dst_path.display(self.path_style()),
                        &["version"],
                        true,
                    )
                    .await
                    .is_ok()
                {
                    return Ok((dst_path.into(), ServerBinaryOutcome::Reused));
                }
                dst_path
            } else {
                dst_path
            };
            let tmp_path = paths::remote_server_dir_relative().join(
                RelPath::from_unix_str(&format!(
                    "download-{}-{}",
                    std::process::id(),
                    remote_server_path.file_name().unwrap().to_string_lossy()
                ))
                .unwrap(),
            );
            self.upload_local_server_binary(&remote_server_path, &tmp_path, delegate, cx)
                .await?;
            self.extract_server_binary(&dst_path, &tmp_path, delegate, cx)
                .await?;
            if matches!(release_channel, ReleaseChannel::Dev) {
                self.remove_stale_dev_server_binaries(&dst_path).await;
            }
            return Ok((dst_path.into(), ServerBinaryOutcome::Uploaded));
        }

        if binary_exists_on_server {
            return Ok((dst_path.into(), ServerBinaryOutcome::Reused));
        }

        let wanted_version = cx.update(|cx| match release_channel {
            ReleaseChannel::Nightly => Ok(None),
            ReleaseChannel::Dev => {
                anyhow::bail!(
                    "ZED_BUILD_REMOTE_SERVER is not set and no remote server exists at ({:?})",
                    dst_path
                )
            }
            _ => Ok(Some(AppVersion::global(cx))),
        })?;

        let tmp_path_compressed = remote_server_dir_relative().join(
            RelPath::from_unix_str(&format!(
                "{}-download-{}.{}",
                binary_name,
                std::process::id(),
                if self.ssh_platform.os.is_windows() {
                    "zip"
                } else {
                    "gz"
                }
            ))
            .unwrap(),
        );
        if !self.socket.connection_options.upload_binary_over_ssh
            && let Some(url) = delegate
                .get_download_url(
                    self.ssh_platform,
                    release_channel,
                    wanted_version.clone(),
                    cx,
                )
                .await?
        {
            match self
                .download_binary_on_server(&url, &tmp_path_compressed, delegate, cx)
                .await
            {
                Ok(_) => {
                    self.extract_server_binary(&dst_path, &tmp_path_compressed, delegate, cx)
                        .await
                        .context("extracting server binary")?;
                    return Ok((dst_path.into(), ServerBinaryOutcome::Uploaded));
                }
                Err(e) => {
                    log::error!(
                        "Failed to download binary on server, attempting to download locally and then upload it the server: {e:#}",
                    )
                }
            }
        }

        let src_path = delegate
            .download_server_binary_locally(
                self.ssh_platform,
                release_channel,
                wanted_version.clone(),
                cx,
            )
            .await
            .context("downloading server binary locally")?;
        self.upload_local_server_binary(&src_path, &tmp_path_compressed, delegate, cx)
            .await
            .context("uploading server binary")?;
        self.extract_server_binary(&dst_path, &tmp_path_compressed, delegate, cx)
            .await
            .context("extracting server binary")?;
        Ok((dst_path.into(), ServerBinaryOutcome::Uploaded))
    }

    /// Best-effort removal of every stale dev server binary after a new one has
    /// been installed successfully. A failure here only costs remote disk.
    #[cfg(any(debug_assertions, feature = "build-remote-server-binary"))]
    async fn remove_stale_dev_server_binaries(&self, keep_path: &RelPath) {
        if self.ssh_platform.os.is_windows() {
            return;
        }
        let shell_kind = ShellKind::Posix;
        let dir = paths::remote_server_dir_relative().display(self.path_style());
        let Some(dir) = shell_kind.try_quote(&dir) else {
            return;
        };
        let keep_path = keep_path.display(self.path_style());
        let Some(keep_path) = shell_kind.try_quote(&keep_path) else {
            return;
        };
        let script = remove_stale_dev_server_binaries_script(&dir, &keep_path);
        let args = shell_kind.args_for_shell(false, script);
        if let Err(error) = self
            .socket
            .run_command(self.ssh_shell_kind, "sh", &args, true)
            .await
        {
            log::warn!("failed to remove stale dev remote server binaries: {error:#}");
        }
    }

    async fn download_binary_on_server(
        &self,
        url: &str,
        tmp_path: &RelPath,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        if let Some(parent) = tmp_path.parent() {
            let res = self
                .socket
                .run_command(
                    self.ssh_shell_kind,
                    "mkdir",
                    &["-p", parent.display(self.path_style()).as_ref()],
                    true,
                )
                .await;
            if !self.ssh_platform.os.is_windows() {
                // mkdir fails on windows if the path already exists ...
                res?;
            }
        }

        delegate.set_status(Some("Downloading remote development server on host"), cx);

        let connection_timeout = self
            .socket
            .connection_options
            .connection_timeout
            .unwrap_or(10)
            .to_string();

        match self
            .socket
            .run_command(
                self.ssh_shell_kind,
                "curl",
                &[
                    "-f",
                    "-L",
                    "--connect-timeout",
                    &connection_timeout,
                    url,
                    "-o",
                    &tmp_path.display(self.path_style()),
                ],
                true,
            )
            .await
        {
            Ok(_) => {}
            Err(e) => {
                if self
                    .socket
                    .run_command(self.ssh_shell_kind, "which", &["curl"], true)
                    .await
                    .is_ok()
                {
                    return Err(e);
                }

                log::info!("curl is not available, trying wget");
                match self
                    .socket
                    .run_command(
                        self.ssh_shell_kind,
                        "wget",
                        &[
                            "--connect-timeout",
                            &connection_timeout,
                            "--tries",
                            "1",
                            url,
                            "-O",
                            &tmp_path.display(self.path_style()),
                        ],
                        true,
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        if self
                            .socket
                            .run_command(self.ssh_shell_kind, "which", &["wget"], true)
                            .await
                            .is_ok()
                        {
                            return Err(e);
                        } else {
                            anyhow::bail!("Neither curl nor wget is available");
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn upload_local_server_binary(
        &self,
        src_path: &Path,
        tmp_path: &RelPath,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        if let Some(parent) = tmp_path.parent() {
            let res = self
                .socket
                .run_command(
                    self.ssh_shell_kind,
                    "mkdir",
                    &["-p", parent.display(self.path_style()).as_ref()],
                    true,
                )
                .await;
            if !self.ssh_platform.os.is_windows() {
                // mkdir fails on windows if the path already exists ...
                res?;
            }
        }

        let src_stat = fs::metadata(&src_path)
            .await
            .with_context(|| format!("failed to get metadata for {:?}", src_path))?;
        let size = src_stat.len();

        let t0 = Instant::now();
        delegate.set_status(Some(&upload_status(size, t0.elapsed())), cx);
        log::info!(
            "uploading remote development server to {:?} ({}kb)",
            tmp_path,
            size / 1024
        );
        let upload = self.upload_file(src_path, tmp_path).fuse();
        futures::pin_mut!(upload);
        loop {
            let timer = cx
                .background_executor()
                .timer(Duration::from_secs(1))
                .fuse();
            futures::pin_mut!(timer);
            select_biased! {
                result = upload => {
                    result.context("failed to upload server binary")?;
                    break;
                }
                () = timer => delegate.set_status(Some(&upload_status(size, t0.elapsed())), cx),
            }
        }
        log::info!("uploaded remote development server in {:?}", t0.elapsed());
        Ok(())
    }

    async fn extract_server_binary(
        &self,
        dst_path: &RelPath,
        tmp_path: &RelPath,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        delegate.set_status(Some("Extracting remote development server"), cx);

        if self.ssh_platform.os.is_windows() {
            self.extract_server_binary_windows(dst_path, tmp_path).await
        } else {
            self.extract_server_binary_posix(dst_path, tmp_path).await
        }
    }

    async fn extract_server_binary_posix(
        &self,
        dst_path: &RelPath,
        tmp_path: &RelPath,
    ) -> Result<()> {
        let shell_kind = ShellKind::Posix;
        let server_mode = 0o755;
        let orig_tmp_path = tmp_path.display(self.path_style());
        let server_mode = format!("{:o}", server_mode);
        let server_mode = shell_kind
            .try_quote(&server_mode)
            .context("shell quoting")?;
        let dst_path = dst_path.display(self.path_style());
        let dst_path = shell_kind.try_quote(&dst_path).context("shell quoting")?;
        let script = if let Some(tmp_path) = orig_tmp_path.strip_suffix(".gz") {
            let orig_tmp_path = shell_kind
                .try_quote(&orig_tmp_path)
                .context("shell quoting")?;
            let tmp_path = shell_kind.try_quote(&tmp_path).context("shell quoting")?;
            format!(
                "gunzip -f {orig_tmp_path} && chmod {server_mode} {tmp_path} && mv {tmp_path} {dst_path}",
            )
        } else {
            let orig_tmp_path = shell_kind
                .try_quote(&orig_tmp_path)
                .context("shell quoting")?;
            format!("chmod {server_mode} {orig_tmp_path} && mv {orig_tmp_path} {dst_path}",)
        };
        let args = shell_kind.args_for_shell(false, script.to_string());
        self.socket
            .run_command(self.ssh_shell_kind, "sh", &args, true)
            .await?;
        Ok(())
    }

    async fn extract_server_binary_windows(
        &self,
        dst_path: &RelPath,
        tmp_path: &RelPath,
    ) -> Result<()> {
        let shell_kind = ShellKind::Pwsh;
        let orig_tmp_path = tmp_path.display(self.path_style());
        let dst_path = dst_path.display(self.path_style());
        let dst_path = shell_kind.try_quote(&dst_path).context("shell quoting")?;

        let script = if let Some(tmp_path) = orig_tmp_path.strip_suffix(".zip") {
            let orig_tmp_path = shell_kind
                .try_quote(&orig_tmp_path)
                .context("shell quoting")?;
            let tmp_path = shell_kind.try_quote(tmp_path).context("shell quoting")?;
            let tmp_exe_path = format!("{tmp_path}\\remote_server.exe");
            let tmp_exe_path = shell_kind
                .try_quote(&tmp_exe_path)
                .context("shell quoting")?;
            format!(
                "Expand-Archive -Force -Path {orig_tmp_path} -DestinationPath {tmp_path} -ErrorAction Stop; Move-Item -Force {tmp_exe_path} {dst_path}; Remove-Item -Force {tmp_path} -Recurse; Remove-Item -Force {orig_tmp_path}",
            )
        } else {
            let orig_tmp_path = shell_kind
                .try_quote(&orig_tmp_path)
                .context("shell quoting")?;
            format!("Move-Item -Force {orig_tmp_path} {dst_path}")
        };

        let args = shell_kind.args_for_shell(false, script);
        self.socket
            .run_command(self.ssh_shell_kind, "powershell", &args, true)
            .await?;
        Ok(())
    }

    fn build_scp_command(
        &self,
        src_path: &Path,
        dest_path_str: &str,
        args: Option<&[&str]>,
    ) -> util::command::Command {
        let mut command = util::command::new_command("scp");
        self.socket.ssh_options(&mut command, false).args(
            self.socket
                .connection_options
                .port
                .map(|port| vec!["-P".to_string(), port.to_string()])
                .unwrap_or_default(),
        );
        if let Some(args) = args {
            command.args(args);
        }
        command.arg(src_path).arg(format!(
            "{}:{}",
            self.socket.connection_options.scp_destination(),
            dest_path_str
        ));
        command
    }

    fn build_sftp_command(&self) -> util::command::Command {
        let mut command = util::command::new_command("sftp");
        self.socket.ssh_options(&mut command, false).args(
            self.socket
                .connection_options
                .port
                .map(|port| vec!["-P".to_string(), port.to_string()])
                .unwrap_or_default(),
        );
        command.arg("-b").arg("-");
        command.arg(self.socket.connection_options.scp_destination());
        command.stdin(Stdio::piped());
        command
    }

    async fn upload_file(&self, src_path: &Path, dest_path: &RelPath) -> Result<()> {
        log::debug!("uploading file {:?} to {:?}", src_path, dest_path);

        let src_path_display = src_path.display().to_string();
        let dest_path_str = dest_path.display(self.path_style());

        // We will try SFTP first, and if that fails, we will fall back to SCP.
        // If SCP fails also, we give up and return an error.
        // The reason we allow a fallback from SFTP to SCP is that if the user has to specify a password,
        // depending on the implementation of SSH stack, SFTP may disable interactive password prompts in batch mode.
        // This is for example the case on Windows as evidenced by this implementation snippet:
        // https://github.com/PowerShell/openssh-portable/blob/b8c08ef9da9450a94a9c5ef717d96a7bd83f3332/sshconnect2.c#L417
        if Self::is_sftp_available().await {
            log::debug!("using SFTP for file upload");
            let mut command = self.build_sftp_command();
            let sftp_batch = format!("put {src_path_display} {dest_path_str}\n");

            let mut child = command.spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                use futures::AsyncWriteExt;
                stdin.write_all(sftp_batch.as_bytes()).await?;
                stdin.flush().await?;
            }

            let output = child.output().await?;
            if output.status.success() {
                return Ok(());
            }

            let stderr = String::from_utf8_lossy(&output.stderr);
            log::debug!(
                "failed to upload file via SFTP {src_path_display} -> {dest_path_str}: {stderr}"
            );
        }

        log::debug!("using SCP for file upload");
        let mut command = self.build_scp_command(src_path, &dest_path_str, None);
        let output = command.output().await?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        log::debug!(
            "failed to upload file via SCP {src_path_display} -> {dest_path_str}: {stderr}",
        );
        anyhow::bail!(
            "failed to upload file via STFP/SCP {} -> {}: {}",
            src_path_display,
            dest_path_str,
            stderr,
        );
    }

    async fn is_sftp_available() -> bool {
        which::which("sftp").is_ok()
    }
}

impl SshSocket {
    #[cfg(not(windows))]
    async fn new(options: SshConnectionOptions, socket_path: PathBuf) -> Result<Self> {
        Ok(Self {
            connection_options: options,
            envs: HashMap::default(),
            socket_path,
        })
    }

    #[cfg(windows)]
    async fn new(
        options: SshConnectionOptions,
        password: askpass::EncryptedPassword,
        executor: gpui::BackgroundExecutor,
    ) -> Result<Self> {
        let mut envs = HashMap::default();
        let get_password =
            move |_| Task::ready(std::ops::ControlFlow::Continue(Ok(password.clone())));

        let _proxy = askpass::PasswordProxy::new(Box::new(get_password), executor).await?;
        envs.insert("SSH_ASKPASS_REQUIRE".into(), "force".into());
        envs.insert(
            "SSH_ASKPASS".into(),
            _proxy.script_path().as_ref().display().to_string(),
        );
        envs.insert(
            "ZED_ASKPASS_SOCKET".into(),
            _proxy.socket_path().as_ref().display().to_string(),
        );

        Ok(Self {
            connection_options: options,
            envs,
            _proxy,
        })
    }

    // :WARNING: ssh unquotes arguments when executing on the remote :WARNING:
    // e.g. $ ssh host sh -c 'ls -l' is equivalent to $ ssh host sh -c ls -l
    // and passes -l as an argument to sh, not to ls.
    // Furthermore, some setups (e.g. Coder) will change directory when SSH'ing
    // into a machine. You must use `cd` to get back to $HOME.
    // You need to do it like this: $ ssh host "cd; sh -c 'ls -l /tmp'"
    fn ssh_command(
        &self,
        shell_kind: ShellKind,
        program: &str,
        args: &[impl AsRef<str>],
        allow_pseudo_tty: bool,
    ) -> util::command::Command {
        let mut command = util::command::new_command("ssh");
        let program = shell_kind.prepend_command_prefix(program);
        let mut to_run = shell_kind
            .try_quote_prefix_aware(&program)
            .expect("shell quoting")
            .into_owned();
        for arg in args {
            // We're trying to work with: sh, bash, zsh, fish, tcsh, ...?
            debug_assert!(
                !arg.as_ref().contains('\n'),
                "multiline arguments do not work in all shells"
            );
            to_run.push(' ');
            to_run.push_str(&shell_kind.try_quote(arg.as_ref()).expect("shell quoting"));
        }
        let to_run = if shell_kind == ShellKind::Cmd {
            to_run // 'cd' prints the current directory in CMD
        } else {
            let separator = shell_kind.sequential_commands_separator();
            format!("cd{separator} {to_run}")
        };
        self.ssh_options(&mut command, true)
            .arg(self.connection_options.ssh_destination());
        if !allow_pseudo_tty {
            command.arg("-T");
        }
        command.arg(to_run);
        log::debug!("ssh {:?}", command);
        command
    }

    async fn run_command(
        &self,
        shell_kind: ShellKind,
        program: &str,
        args: &[impl AsRef<str>],
        allow_pseudo_tty: bool,
    ) -> Result<String> {
        let mut command = self.ssh_command(shell_kind, program, args, allow_pseudo_tty);
        let output = command.output().await?;
        log::debug!("{:?}: {:?}", command, output);
        anyhow::ensure!(
            output.status.success(),
            "failed to run command {command:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn ssh_options<'a>(
        &self,
        command: &'a mut util::command::Command,
        include_port_forwards: bool,
    ) -> &'a mut util::command::Command {
        let args = if include_port_forwards {
            self.connection_options
                .additional_args_without_structured_forwards()
        } else {
            self.connection_options.additional_args_for_scp()
        };

        let cmd = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args(["-o", "ClearAllForwardings=yes"]);

        if cfg!(windows) {
            cmd.envs(self.envs.clone());
        }
        #[cfg(not(windows))]
        {
            cmd.args(["-o", "ControlMaster=no", "-o"])
                .arg(format!("ControlPath={}", self.socket_path.display()));
        }
        cmd.args(args);
        cmd
    }

    // Returns the SSH command-line options (without the destination) for building commands.
    // On Linux, this includes the ControlPath option to reuse the existing connection.
    // Note: The destination must be added separately after all options to ensure proper
    // SSH command structure: ssh [options] destination [command]
    fn ssh_command_options(&self) -> Vec<String> {
        let mut arguments = vec!["-o".to_owned(), "ClearAllForwardings=yes".to_owned()];
        #[cfg(not(windows))]
        arguments.extend([
            "-o".to_owned(),
            "ControlMaster=no".to_owned(),
            "-o".to_owned(),
            format!("ControlPath={}", self.socket_path.display()),
        ]);
        arguments.extend(
            self.connection_options
                .additional_args_without_structured_forwards(),
        );
        arguments
    }

    fn ssh_command_options_for_explicit_forward(&self) -> Vec<String> {
        #[cfg(not(windows))]
        {
            vec![
                "-F".to_owned(),
                "/dev/null".to_owned(),
                "-o".to_string(),
                "ControlMaster=no".to_string(),
                "-o".to_string(),
                format!("ControlPath={}", self.socket_path.display()),
            ]
        }
        #[cfg(windows)]
        {
            self.connection_options
                .additional_args_without_structured_forwards()
        }
    }

    async fn platform(&self, shell: ShellKind, is_windows: bool) -> Result<RemotePlatform> {
        if is_windows {
            self.platform_windows(shell).await
        } else {
            self.platform_posix(shell).await
        }
    }

    async fn platform_posix(&self, shell: ShellKind) -> Result<RemotePlatform> {
        let output = self
            .run_command(shell, "uname", &["-sm"], false)
            .await
            .context("Failed to run 'uname -sm' to determine platform")?;
        parse_platform(&output)
    }

    /// Best-effort detection of the remote OS version. Failures are logged and
    /// result in `None` rather than failing the connection, since this is only
    /// used for telemetry.
    async fn os_version(&self, os: RemoteOs, shell: ShellKind) -> Option<String> {
        let (program, args) = super::os_version_command(os);
        match self.run_command(shell, program, args, false).await {
            Ok(output) => super::parse_os_version(os, &output),
            Err(error) => {
                log::warn!("Failed to determine remote OS version: {error:#}");
                None
            }
        }
    }

    async fn platform_windows(&self, shell: ShellKind) -> Result<RemotePlatform> {
        let output = self
            .run_command(
                shell,
                "cmd.exe",
                &["/c", "echo", "%PROCESSOR_ARCHITECTURE%"],
                false,
            )
            .await
            .context(
                "Failed to run 'echo %PROCESSOR_ARCHITECTURE%' to determine Windows architecture",
            )?;

        Ok(RemotePlatform {
            os: RemoteOs::Windows,
            arch: match output.trim() {
                "AMD64" => RemoteArch::X86_64,
                "ARM64" => RemoteArch::Aarch64,
                arch => anyhow::bail!(
                    "Prebuilt remote servers are not yet available for windows-{arch}. See https://zed.dev/docs/remote-development"
                ),
            },
        })
    }

    /// Probes whether the remote host is running Windows.
    ///
    /// This is done by attempting to run a simple Windows-specific command.
    /// If it succeeds and returns Windows-like output, we assume it's Windows.
    async fn probe_is_windows(&self) -> bool {
        match self
            .run_command(ShellKind::Cmd, "cmd.exe", &["/c", "ver"], false)
            .await
        {
            // Windows 'ver' command outputs something like "Microsoft Windows [Version 10.0.19045.5011]"
            Ok(output) => output.trim().contains("indows"),
            Err(_) => false,
        }
    }

    async fn shell(&self, is_windows: bool) -> String {
        if is_windows {
            self.shell_windows().await
        } else {
            self.shell_posix().await
        }
    }

    async fn shell_posix(&self) -> String {
        const DEFAULT_SHELL: &str = "sh";
        match self
            .run_command(ShellKind::Posix, "sh", &["-c", "echo $SHELL"], false)
            .await
        {
            Ok(output) => parse_shell(&output, DEFAULT_SHELL),
            Err(e) => {
                log::error!("Failed to detect remote shell: {e}");
                DEFAULT_SHELL.to_owned()
            }
        }
    }

    async fn shell_windows(&self) -> String {
        const DEFAULT_SHELL: &str = "cmd.exe";

        // We detect the shell used by the SSH session by running the following command in PowerShell:
        // (Get-CimInstance Win32_Process -Filter "ProcessId = $((Get-CimInstance Win32_Process -Filter ProcessId=$PID).ParentProcessId)").Name
        // This prints the name of PowerShell's parent process (which will be the shell that SSH launched).
        // We pass it as a Base64 encoded string since we don't yet know how to correctly quote that command.
        // (We'd need to know what the shell is to do that...)
        match self
            .run_command(
                ShellKind::Cmd,
                "powershell",
                &[
                    "-E",
                    "KABHAGUAdAAtAEMAaQBtAEkAbgBzAHQAYQBuAGMAZQAgAFcAaQBuADMAMgBfAFAAcgBvAGMAZQBzAHMAIAAtAEYAaQBsAHQAZQByACAAIgBQAHIAbwBjAGUAcwBzAEkAZAAgAD0AIAAkACgAKABHAGUAdAAtAEMAaQBtAEkAbgBzAHQAYQBuAGMAZQAgAFcAaQBuADMAMgBfAFAAcgBvAGMAZQBzAHMAIAAtAEYAaQBsAHQAZQByACAAUAByAG8AYwBlAHMAcwBJAGQAPQAkAFAASQBEACkALgBQAGEAcgBlAG4AdABQAHIAbwBjAGUAcwBzAEkAZAApACIAKQAuAE4AYQBtAGUA",
                ],
                false,
            )
            .await
        {
            Ok(output) => parse_shell(&output, DEFAULT_SHELL),
            Err(e) => {
                log::error!("Failed to detect remote shell: {e}");
                DEFAULT_SHELL.to_owned()
            }
        }
    }
}

fn parse_port_number(port_str: &str) -> Result<u16> {
    port_str
        .parse()
        .with_context(|| format!("parsing port number: {port_str}"))
}

fn split_port_forward_tokens(spec: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut chars = spec.chars().peekable();

    while chars.peek().is_some() {
        if chars.peek() == Some(&'[') {
            chars.next();
            let mut bracket_content = String::new();
            loop {
                match chars.next() {
                    Some(']') => break,
                    Some(ch) => bracket_content.push(ch),
                    None => anyhow::bail!("Unmatched '[' in port forward spec: {spec}"),
                }
            }
            tokens.push(bracket_content);
            if chars.peek() == Some(&':') {
                chars.next();
            }
        } else {
            let mut token = String::new();
            for ch in chars.by_ref() {
                if ch == ':' {
                    break;
                }
                token.push(ch);
            }
            tokens.push(token);
        }
    }

    Ok(tokens)
}

fn parse_port_forward_spec(spec: &str) -> Result<SshPortForwardOption> {
    let tokens = if spec.contains('[') {
        split_port_forward_tokens(spec)?
    } else {
        spec.split(':').map(String::from).collect()
    };

    match tokens.len() {
        4 => {
            let local_port = parse_port_number(&tokens[1])?;
            let remote_port = parse_port_number(&tokens[3])?;

            Ok(SshPortForwardOption {
                local_host: Some(tokens[0].clone()),
                local_port,
                remote_host: Some(tokens[2].clone()),
                remote_port,
            })
        }
        3 => {
            let local_port = parse_port_number(&tokens[0])?;
            let remote_port = parse_port_number(&tokens[2])?;

            Ok(SshPortForwardOption {
                local_host: None,
                local_port,
                remote_host: Some(tokens[1].clone()),
                remote_port,
            })
        }
        _ => anyhow::bail!("Invalid port forward format: {spec}"),
    }
}

impl SshConnectionOptions {
    pub fn parse_command_line(input: &str) -> Result<Self> {
        let input = input.trim_start_matches("ssh ");
        let mut hostname: Option<String> = None;
        let mut username: Option<String> = None;
        let mut port: Option<u16> = None;
        let mut args = Vec::new();
        let mut port_forwards: Vec<SshPortForwardOption> = Vec::new();

        // disallowed: -E, -e, -F, -f, -G, -g, -M, -N, -n, -O, -q, -S, -s, -T, -t, -V, -v, -W
        const ALLOWED_OPTS: &[&str] = &[
            "-4", "-6", "-A", "-a", "-C", "-K", "-k", "-X", "-x", "-Y", "-y",
        ];
        const ALLOWED_ARGS: &[&str] = &[
            "-B", "-b", "-c", "-D", "-F", "-I", "-i", "-J", "-l", "-m", "-o", "-P", "-p", "-R",
            "-w",
        ];

        let mut tokens = ShellKind::Posix
            .split(input)
            .context("invalid input")?
            .into_iter();

        'outer: while let Some(arg) = tokens.next() {
            if ALLOWED_OPTS.contains(&(&arg as &str)) {
                args.push(arg.to_string());
                continue;
            }
            if arg == "-p" {
                port = tokens.next().and_then(|arg| arg.parse().ok());
                continue;
            } else if let Some(p) = arg.strip_prefix("-p") {
                port = p.parse().ok();
                continue;
            }
            if arg == "-l" {
                username = tokens.next();
                continue;
            } else if let Some(l) = arg.strip_prefix("-l") {
                username = Some(l.to_string());
                continue;
            }
            if arg == "-L" || arg.starts_with("-L") {
                let forward_spec = if arg == "-L" {
                    tokens.next()
                } else {
                    Some(arg.strip_prefix("-L").unwrap().to_string())
                };

                if let Some(spec) = forward_spec {
                    port_forwards.push(parse_port_forward_spec(&spec)?);
                } else {
                    anyhow::bail!("Missing port forward format");
                }
            }

            for a in ALLOWED_ARGS {
                if arg == *a {
                    args.push(arg);
                    if let Some(next) = tokens.next() {
                        args.push(next);
                    }
                    continue 'outer;
                } else if arg.starts_with(a) {
                    args.push(arg);
                    continue 'outer;
                }
            }
            if arg.starts_with("-") || hostname.is_some() {
                anyhow::bail!("unsupported argument: {:?}", arg);
            }
            let mut input = &arg as &str;
            // Destination might be: username1@username2@ip2@ip1
            if let Some((u, rest)) = input.rsplit_once('@') {
                input = rest;
                username = Some(u.to_string());
            }

            // Handle port parsing, accounting for IPv6 addresses
            // IPv6 addresses can be: 2001:db8::1 or [2001:db8::1]:22
            if input.starts_with('[') {
                if let Some((rest, p)) = input.rsplit_once("]:") {
                    input = rest.strip_prefix('[').unwrap_or(rest);
                    port = p.parse().ok();
                } else if input.ends_with(']') {
                    input = input.strip_prefix('[').unwrap_or(input);
                    input = input.strip_suffix(']').unwrap_or(input);
                }
            } else if let Some((rest, p)) = input.rsplit_once(':')
                && !rest.contains(":")
            {
                input = rest;
                port = p.parse().ok();
            }

            hostname = Some(input.to_string())
        }

        let Some(hostname) = hostname else {
            anyhow::bail!("missing hostname");
        };

        let port_forwards = match port_forwards.len() {
            0 => None,
            _ => Some(port_forwards),
        };

        Ok(Self {
            host: hostname.into(),
            username,
            port,
            port_forwards,
            args: Some(args),
            password: None,
            nickname: None,
            upload_binary_over_ssh: false,
            connection_timeout: None,
        })
    }

    pub fn ssh_destination(&self) -> String {
        let mut result = String::default();
        if let Some(username) = &self.username {
            // Username might be: username1@username2@ip2
            let username = urlencoding::encode(username);
            result.push_str(&username);
            result.push('@');
        }

        result.push_str(&self.host.to_string());
        result
    }

    pub fn additional_args_for_scp(&self) -> Vec<String> {
        self.args.iter().flatten().cloned().collect::<Vec<String>>()
    }

    fn additional_args_without_structured_forwards(&self) -> Vec<String> {
        let mut args = self.additional_args_for_scp();

        if let Some(timeout) = self.connection_timeout {
            args.extend(["-o".to_string(), format!("ConnectTimeout={}", timeout)]);
        }

        if let Some(port) = self.port {
            args.push("-p".to_string());
            args.push(port.to_string());
        }

        args
    }

    pub fn additional_args(&self) -> Vec<String> {
        let mut args = self.additional_args_without_structured_forwards();

        if let Some(forwards) = &self.port_forwards {
            args.extend(forwards.iter().map(|pf| {
                let local_host = match &pf.local_host {
                    Some(host) => host,
                    None => "localhost",
                };
                let remote_host = match &pf.remote_host {
                    Some(host) => host,
                    None => "localhost",
                };

                format!(
                    "-L{}:{}:{}:{}",
                    bracket_ipv6(local_host),
                    pf.local_port,
                    bracket_ipv6(remote_host),
                    pf.remote_port
                )
            }));
        }

        args
    }

    fn scp_destination(&self) -> String {
        if let Some(username) = &self.username {
            format!("{}@{}", username, self.host.to_bracketed_string())
        } else {
            self.host.to_string()
        }
    }

    pub fn connection_string(&self) -> String {
        let host = if let Some(port) = &self.port {
            format!("{}:{}", self.host.to_bracketed_string(), port)
        } else {
            self.host.to_string()
        };

        if let Some(username) = &self.username {
            format!("{}@{}", username, host)
        } else {
            host
        }
    }
}

#[cfg(not(windows))]
fn ssh_args_have_forwards(arguments: &[String]) -> bool {
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        if matches!(argument.as_str(), "-L" | "-R" | "-D")
            || ["-L", "-R", "-D"]
                .iter()
                .any(|flag| argument.starts_with(flag) && argument.len() > flag.len())
        {
            return true;
        }
        let option = if argument == "-o" {
            arguments.next().map(String::as_str)
        } else {
            argument.strip_prefix("-o")
        };
        if option.is_some_and(|option| {
            let option = option.to_ascii_lowercase();
            ["localforward", "remoteforward", "dynamicforward"]
                .iter()
                .any(|name| {
                    option == *name
                        || option
                            .strip_prefix(name)
                            .is_some_and(|rest| rest.starts_with(['=', ' ']))
                })
        }) {
            return true;
        }
    }
    false
}

fn build_command_posix(
    input_program: Option<String>,
    input_args: &[String],
    input_env: &HashMap<String, String>,
    working_dir: Option<String>,
    port_forward: Option<(u16, String, u16)>,
    ssh_env: HashMap<String, String>,
    ssh_path_style: PathStyle,
    ssh_shell: &str,
    ssh_shell_kind: ShellKind,
    ssh_options: Vec<String>,
    ssh_destination: &str,
    interactive: Interactive,
) -> Result<CommandTemplate> {
    use std::fmt::Write as _;

    let mut exec = String::new();
    if let Some(working_dir) = working_dir {
        let working_dir = RemotePathBuf::new(working_dir, ssh_path_style).to_string();

        // For paths starting with ~/, we need $HOME to expand, but the remainder
        // must be properly quoted to prevent command injection.
        // Pattern: cd "$HOME"/'quoted/remainder' - $HOME expands, rest is single-quoted
        const TILDE_PREFIX: &str = "~/";
        if working_dir.starts_with(TILDE_PREFIX) {
            let remainder = working_dir.trim_start_matches(TILDE_PREFIX);
            if remainder.is_empty() {
                write!(
                    exec,
                    "cd \"$HOME\" {} ",
                    ssh_shell_kind.sequential_and_commands_separator()
                )?;
            } else {
                let quoted_remainder = ssh_shell_kind
                    .try_quote(remainder)
                    .context("shell quoting")?;
                write!(
                    exec,
                    "cd \"$HOME\"/{quoted_remainder} {} ",
                    ssh_shell_kind.sequential_and_commands_separator()
                )?;
            }
        } else {
            let quoted_dir = ssh_shell_kind
                .try_quote(&working_dir)
                .context("shell quoting")?;
            write!(
                exec,
                "cd {quoted_dir} {} ",
                ssh_shell_kind.sequential_and_commands_separator()
            )?;
        }
    } else {
        write!(
            exec,
            "cd {} ",
            ssh_shell_kind.sequential_and_commands_separator()
        )?;
    };
    write!(exec, "exec env ")?;

    for (k, v) in input_env.iter() {
        let assignment = format!("{k}={v}");
        let assignment = ssh_shell_kind
            .try_quote(&assignment)
            .context("shell quoting")?;
        write!(exec, "{assignment} ")?;
    }

    if let Some(input_program) = input_program {
        write!(
            exec,
            "{}",
            ssh_shell_kind
                .try_quote_prefix_aware(&input_program)
                .context("shell quoting")?
        )?;
        for arg in input_args {
            let arg = ssh_shell_kind.try_quote(&arg).context("shell quoting")?;
            write!(exec, " {}", &arg)?;
        }
    } else {
        write!(exec, "{ssh_shell} -l")?;
    };

    let mut args = Vec::new();
    args.extend(ssh_options);

    if let Some((local_port, host, remote_port)) = port_forward {
        args.push("-L".into());
        args.push(format!(
            "{}:{}:{}",
            local_port,
            bracket_ipv6(&host),
            remote_port
        ));
    }

    // LogLevel=ERROR suppresses the "Connection to ... closed." message while
    // preserving SSH errors.
    args.extend(["-o".into(), "LogLevel=ERROR".into()]);
    match interactive {
        // -t forces pseudo-TTY allocation (for interactive use)
        Interactive::Yes => args.push("-t".into()),
        // -T disables pseudo-TTY allocation (for non-interactive piped stdio)
        Interactive::No => args.push("-T".into()),
    }
    // The destination must come after all options but before the command
    args.push(ssh_destination.into());
    args.push(exec);

    Ok(CommandTemplate {
        program: "ssh".into(),
        args,
        env: ssh_env,
    })
}

fn build_command_windows(
    input_program: Option<String>,
    input_args: &[String],
    _input_env: &HashMap<String, String>,
    working_dir: Option<String>,
    port_forward: Option<(u16, String, u16)>,
    ssh_env: HashMap<String, String>,
    ssh_path_style: PathStyle,
    ssh_shell: &str,
    _ssh_shell_kind: ShellKind,
    ssh_options: Vec<String>,
    ssh_destination: &str,
    interactive: Interactive,
) -> Result<CommandTemplate> {
    use base64::Engine as _;
    use std::fmt::Write as _;

    let mut exec = String::new();
    let shell_kind = ShellKind::PowerShell;

    if let Some(working_dir) = working_dir {
        let working_dir = RemotePathBuf::new(working_dir, ssh_path_style).to_string();

        write!(
            exec,
            "Set-Location -Path {} {} ",
            shell_kind
                .try_quote(&working_dir)
                .context("shell quoting")?,
            shell_kind.sequential_and_commands_separator()
        )?;
    }

    // Windows OpenSSH has an 8K character limit for command lines. Sending a lot of environment variables easily puts us over the limit.
    // Until we have a better solution for this, we just won't set environment variables for now.
    // for (k, v) in input_env.iter() {
    //     write!(
    //         exec,
    //         "$env:{}={} {} ",
    //         k,
    //         shell_kind.try_quote(v).context("shell quoting")?,
    //         shell_kind.sequential_and_commands_separator()
    //     )?;
    // }

    if let Some(input_program) = input_program {
        write!(
            exec,
            "{}",
            shell_kind
                .try_quote_prefix_aware(&shell_kind.prepend_command_prefix(&input_program))
                .context("shell quoting")?
        )?;
        for arg in input_args {
            let arg = shell_kind.try_quote(arg).context("shell quoting")?;
            write!(exec, " {}", &arg)?;
        }
    } else {
        // Launch an interactive shell session
        write!(exec, "{ssh_shell}")?;
    };

    let mut args = Vec::new();
    args.extend(ssh_options);

    if let Some((local_port, host, remote_port)) = port_forward {
        args.push("-L".into());
        args.push(format!(
            "{}:{}:{}",
            local_port,
            bracket_ipv6(&host),
            remote_port
        ));
    }

    // LogLevel=ERROR suppresses the "Connection to ... closed." message while
    // preserving SSH errors.
    args.extend(["-o".into(), "LogLevel=ERROR".into()]);
    match interactive {
        // -t forces pseudo-TTY allocation (for interactive use)
        Interactive::Yes => args.push("-t".into()),
        // -T disables pseudo-TTY allocation (for non-interactive piped stdio)
        Interactive::No => args.push("-T".into()),
    }

    // The destination must come after all options but before the command
    args.push(ssh_destination.into());

    // Windows OpenSSH server incorrectly escapes the command string when the PTY is used.
    // The simplest way to work around this is to use a base64 encoded command, which doesn't require escaping.
    let utf16_bytes: Vec<u16> = exec.encode_utf16().collect();
    let byte_slice: Vec<u8> = utf16_bytes.iter().flat_map(|&u| u.to_le_bytes()).collect();
    let base64_encoded = base64::engine::general_purpose::STANDARD.encode(&byte_slice);

    args.push(format!("powershell.exe -E {}", base64_encoded));

    Ok(CommandTemplate {
        program: "ssh".into(),
        args,
        env: ssh_env,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_build_id_uses_top_level_app_identity() {
        let clean = AppCommitSha::new("123456789abcdef0".to_string(), false);
        let dirty = AppCommitSha::new("123456789abcdef0".to_string(), true);

        assert_eq!(
            dev_build_id(Some(&clean)).map(|id| (id.version, id.clean)),
            Some(("build-123456789abc".to_string(), true))
        );
        assert_eq!(
            dev_build_id(Some(&dirty)).map(|id| (id.version, id.clean)),
            Some(("build-123456789abc-dirty".to_string(), false))
        );
        assert!(dev_build_id(None).is_none());
    }

    #[test]
    fn dirty_dev_server_binary_name_includes_artifact_hash() -> Result<()> {
        let dirty = AppCommitSha::new("123456789abcdef0".to_string(), true);
        let dev_build_id = dev_build_id(Some(&dirty)).context("missing dev build identity")?;
        let version = Version::new(1, 0, 0);

        let first = server_binary_name(
            ReleaseChannel::Dev,
            &version,
            Some(&dev_build_id),
            Some("first-hash"),
            false,
        );
        let repeated = server_binary_name(
            ReleaseChannel::Dev,
            &version,
            Some(&dev_build_id),
            Some("first-hash"),
            false,
        );
        let changed = server_binary_name(
            ReleaseChannel::Dev,
            &version,
            Some(&dev_build_id),
            Some("second-hash"),
            false,
        );

        assert_eq!(first, repeated);
        assert_ne!(first, changed);
        assert_eq!(
            first,
            "zed-remote-server-dev-build-123456789abc-dirty-first-hash"
        );
        Ok(())
    }

    #[cfg(not(windows))]
    #[test]
    fn owned_master_keeps_configured_forwards_off_multiplexed_children() {
        let options = SshConnectionOptions {
            host: "example.com".into(),
            port: Some(2222),
            args: Some(vec![
                "-o".to_owned(),
                "ClearAllForwardings=no".to_owned(),
                "-L6001:localhost:6001".to_owned(),
            ]),
            port_forwards: Some(vec![SshPortForwardOption {
                local_host: Some("127.0.0.1".to_owned()),
                local_port: 5001,
                remote_host: Some("127.0.0.1".to_owned()),
                remote_port: 5001,
            }]),
            ..Default::default()
        };
        let socket = SshSocket {
            connection_options: options.clone(),
            socket_path: PathBuf::from("/tmp/ssh.sock"),
            envs: HashMap::default(),
        };
        let master = MasterProcess::command(
            std::ffi::OsStr::new("askpass"),
            options.additional_args(),
            Path::new("/tmp/ssh.sock"),
            "example.com",
        );
        let master_args = master
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(
            master_args
                .iter()
                .any(|arg| arg == "-L127.0.0.1:5001:127.0.0.1:5001"),
            "the owning master must establish configured forwards"
        );
        assert!(
            !socket
                .ssh_command_options()
                .iter()
                .any(|arg| arg == "-L127.0.0.1:5001:127.0.0.1:5001"),
            "structured forwards belong only to the master"
        );
        assert!(
            !socket
                .ssh_command_options_for_explicit_forward()
                .iter()
                .any(|arg| arg == "-L6001:localhost:6001"),
            "explicit forwarding children must not repeat raw forwards"
        );
        assert!(
            socket
                .ssh_command_options()
                .windows(2)
                .any(|args| args == ["-o", "ClearAllForwardings=yes"]),
            "raw arguments and ssh config forwards must also be disabled"
        );
        assert!(
            !socket
                .ssh_command_options_for_explicit_forward()
                .windows(2)
                .any(|args| args == ["-o", "ClearAllForwardings=yes"]),
            "an explicit forwarding child must keep its own -L arguments"
        );
        assert!(
            socket
                .ssh_command_options_for_explicit_forward()
                .windows(2)
                .any(|args| args == ["-F", "/dev/null"]),
            "ssh config forwards must not leak into an explicit forwarding child"
        );
        assert!(
            socket
                .ssh_command_options()
                .windows(2)
                .any(|args| args == ["-p", "2222"]),
            "ordinary connection options must remain"
        );

        let mut command = util::command::new_command("ssh");
        command
            .args(socket.ssh_command_options())
            .args(["-G", "example.invalid"]);
        let output =
            smol::block_on(command.output()).expect("resolving effective child SSH configuration");
        assert!(output.status.success());
        let effective_config = String::from_utf8_lossy(&output.stdout);
        assert!(effective_config.contains("clearallforwardings yes\n"));
        assert!(!effective_config.lines().any(|line| {
            line.starts_with("localforward ")
                || line.starts_with("remoteforward ")
                || line.starts_with("dynamicforward ")
        }));
    }

    #[cfg(not(windows))]
    #[test]
    fn command_line_forwards_require_an_owned_master() {
        for arguments in [
            vec!["-L", "6001:localhost:6001"],
            vec!["-R6002:localhost:6002"],
            vec!["-D", "6003"],
            vec!["-o", "LocalForward=6004 localhost:6004"],
            vec!["-oRemoteForward 6005 localhost:6005"],
            vec!["-o", "DynamicForward 6006"],
        ] {
            let arguments = arguments.into_iter().map(String::from).collect::<Vec<_>>();
            assert!(ssh_args_have_forwards(&arguments), "{arguments:?}");
        }
        assert!(!ssh_args_have_forwards(&[
            "-o".to_owned(),
            "ClearAllForwardings=yes".to_owned(),
            "-p".to_owned(),
            "2222".to_owned(),
        ]));
    }

    #[cfg(not(windows))]
    #[test]
    fn one_off_forward_is_not_cleared_from_the_child_command() -> Result<()> {
        let connection = SshRemoteConnection {
            socket: SshSocket {
                connection_options: SshConnectionOptions {
                    host: "example.com".into(),
                    port_forwards: Some(vec![SshPortForwardOption {
                        local_host: Some("127.0.0.1".to_owned()),
                        local_port: 5001,
                        remote_host: Some("127.0.0.1".to_owned()),
                        remote_port: 5001,
                    }]),
                    ..Default::default()
                },
                socket_path: PathBuf::from("/tmp/ssh.sock"),
                envs: HashMap::default(),
            },
            master_process: Mutex::new(None),
            killed: AtomicBool::new(false),
            remote_binary_path: None,
            ssh_platform: RemotePlatform {
                os: RemoteOs::Linux,
                arch: RemoteArch::Aarch64,
            },
            ssh_os_version: None,
            ssh_path_style: PathStyle::Unix,
            ssh_shell: "/bin/sh".to_owned(),
            ssh_shell_kind: ShellKind::Posix,
            ssh_default_system_shell: "/bin/sh".to_owned(),
            _temp_dir: tempfile::tempdir()?,
        };

        let command = connection.build_command(
            Some("true".to_owned()),
            &[],
            &HashMap::default(),
            None,
            Some((7001, "127.0.0.1".to_owned(), 7002)),
            Interactive::No,
        )?;
        assert!(
            command
                .args
                .windows(2)
                .any(|args| args == ["-L", "7001:127.0.0.1:7002"]),
            "the requested one-off forward is missing: {:?}",
            command.args
        );
        assert!(
            !command
                .args
                .iter()
                .any(|arg| arg == "ClearAllForwardings=yes"),
            "ClearAllForwardings would silently disable the requested -L"
        );
        assert!(
            !command.args.iter().any(|arg| arg.contains(":5001:")),
            "connection-level forwards must stay on the master"
        );
        Ok(())
    }

    #[cfg(not(windows))]
    #[test]
    fn a_missing_control_master_is_not_usable() {
        let directory = tempfile::tempdir().expect("creating test directory");
        assert!(!smol::block_on(control_master_is_alive(
            &directory.path().join("missing.sock")
        )));
    }

    #[cfg(not(windows))]
    #[test]
    fn an_unresponsive_control_master_check_times_out() {
        let directory = tempfile::tempdir().expect("creating test directory");
        let socket_path = directory.path().join("silent.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket_path)
            .expect("binding silent control socket");
        let started = Instant::now();
        let check =
            std::thread::spawn(move || smol::block_on(control_master_is_alive(&socket_path)));
        let _connection = listener.accept().expect("accepting SSH control check");

        assert!(!check.join().expect("joining SSH control check"));
        assert!(started.elapsed() < CONTROL_MASTER_CHECK_TIMEOUT * 3);
    }

    #[cfg(not(windows))]
    #[gpui::test]
    async fn draining_master_stderr_prevents_pipe_backpressure(cx: &mut gpui::TestAppContext) {
        let payload_bytes = 1024 * 1024;
        let mut command = util::command::new_command("sh");
        command
            .args([
                "-c",
                &format!("dd if=/dev/zero bs={payload_bytes} count=1 >&2"),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let process = command.spawn().expect("spawning stand-in master");
        let mut master = MasterProcess {
            process,
            _stderr_task: None,
        };
        master
            .drain_stderr(&cx.background_executor)
            .expect("starting the stderr drain");

        let status_task = cx.background_executor.spawn(async move {
            master
                .process
                .status()
                .await
                .expect("waiting for stand-in master")
        });
        cx.background_executor.allow_parking();
        cx.run_until_parked();
        let status = status_task.await;
        assert!(status.success());
    }

    #[test]
    fn stale_dev_server_cleanup_keeps_new_binary() {
        assert_eq!(
            remove_stale_dev_server_binaries_script("'.zed_server'", "'.zed_server/current'"),
            "for file in '.zed_server'/zed-remote-server-dev-build*; do [ \"$file\" = '.zed_server/current' ] || rm -f \"$file\"; done"
        );
    }

    #[test]
    fn upload_status_reports_size_and_elapsed_time() {
        let size = 46 * 1024 * 1024 + 1;

        assert_eq!(
            upload_status(size, Duration::ZERO),
            "Uploading remote development server (47 MiB, 0s)"
        );
        assert_eq!(
            upload_status(size, Duration::from_secs(12)),
            "Uploading remote development server (47 MiB, 12s)"
        );
    }

    #[test]
    fn test_build_command() -> Result<()> {
        let mut input_env = HashMap::default();
        input_env.insert("INPUT_VA".to_string(), "val".to_string());
        let mut env = HashMap::default();
        env.insert("SSH_VAR".to_string(), "ssh-val".to_string());

        // Test non-interactive command (interactive=false should use -T)
        let command = build_command_posix(
            Some("remote_program".to_string()),
            &["arg1".to_string(), "arg2".to_string()],
            &input_env,
            Some("~/work".to_string()),
            None,
            env.clone(),
            PathStyle::Unix,
            "/bin/bash",
            ShellKind::Posix,
            vec!["-o".to_string(), "ControlMaster=auto".to_string()],
            "user@host",
            Interactive::No,
        )?;
        assert_eq!(command.program, "ssh");
        // Should contain -T for non-interactive
        assert!(command.args.iter().any(|arg| arg == "-T"));
        assert!(!command.args.iter().any(|arg| arg == "-t"));

        // Test interactive command (interactive=true should use -t)
        let command = build_command_posix(
            Some("remote_program".to_string()),
            &["arg1".to_string(), "arg2".to_string()],
            &input_env,
            Some("~/work".to_string()),
            None,
            env.clone(),
            PathStyle::Unix,
            "/bin/fish",
            ShellKind::Fish,
            vec!["-p".to_string(), "2222".to_string()],
            "user@host",
            Interactive::Yes,
        )?;

        assert_eq!(command.program, "ssh");
        assert_eq!(
            command.args.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "-p",
                "2222",
                "-o",
                "LogLevel=ERROR",
                "-t",
                "user@host",
                "cd \"$HOME\"/work && exec env 'INPUT_VA=val' remote_program arg1 arg2"
            ]
        );
        assert_eq!(command.env, env);

        let mut input_env = HashMap::default();
        input_env.insert("INPUT_VA".to_string(), "val".to_string());
        let mut env = HashMap::default();
        env.insert("SSH_VAR".to_string(), "ssh-val".to_string());

        let command = build_command_posix(
            None,
            &[],
            &input_env,
            None,
            Some((1, "foo".to_owned(), 2)),
            env.clone(),
            PathStyle::Unix,
            "/bin/fish",
            ShellKind::Fish,
            vec!["-p".to_string(), "2222".to_string()],
            "user@host",
            Interactive::Yes,
        )?;

        assert_eq!(command.program, "ssh");
        assert_eq!(
            command.args.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "-p",
                "2222",
                "-L",
                "1:foo:2",
                "-o",
                "LogLevel=ERROR",
                "-t",
                "user@host",
                "cd && exec env 'INPUT_VA=val' /bin/fish -l"
            ]
        );
        assert_eq!(command.env, env);

        Ok(())
    }

    #[test]
    fn test_build_command_quotes_env_assignment() -> Result<()> {
        let mut input_env = HashMap::default();
        input_env.insert("ZED$(echo foo)".to_string(), "value".to_string());

        let command = build_command_posix(
            Some("remote_program".to_string()),
            &[],
            &input_env,
            None,
            None,
            HashMap::default(),
            PathStyle::Unix,
            "/bin/bash",
            ShellKind::Posix,
            vec![],
            "user@host",
            Interactive::No,
        )?;

        let remote_command = command
            .args
            .last()
            .context("missing remote command argument")?;
        assert!(
            remote_command.contains("exec env 'ZED$(echo foo)=value' remote_program"),
            "expected env assignment to be quoted, got: {remote_command}"
        );

        Ok(())
    }

    #[test]
    fn scp_args_exclude_port_forward_flags() {
        let options = SshConnectionOptions {
            host: "example.com".into(),
            args: Some(vec![
                "-p".to_string(),
                "2222".to_string(),
                "-o".to_string(),
                "StrictHostKeyChecking=no".to_string(),
            ]),
            port_forwards: Some(vec![SshPortForwardOption {
                local_host: Some("127.0.0.1".to_string()),
                local_port: 8080,
                remote_host: Some("127.0.0.1".to_string()),
                remote_port: 80,
            }]),
            ..Default::default()
        };

        let ssh_args = options.additional_args();
        assert!(
            ssh_args.iter().any(|arg| arg.starts_with("-L")),
            "expected ssh args to include port-forward: {ssh_args:?}"
        );

        let scp_args = options.additional_args_for_scp();
        assert_eq!(
            scp_args,
            vec![
                "-p".to_string(),
                "2222".to_string(),
                "-o".to_string(),
                "StrictHostKeyChecking=no".to_string(),
            ]
        );
    }

    #[test]
    fn test_host_parsing() -> Result<()> {
        let opts = SshConnectionOptions::parse_command_line("user@2001:db8::1")?;
        assert_eq!(opts.host, "2001:db8::1".into());
        assert_eq!(opts.username, Some("user".to_string()));
        assert_eq!(opts.port, None);

        let opts = SshConnectionOptions::parse_command_line("user@[2001:db8::1]:2222")?;
        assert_eq!(opts.host, "2001:db8::1".into());
        assert_eq!(opts.username, Some("user".to_string()));
        assert_eq!(opts.port, Some(2222));

        let opts = SshConnectionOptions::parse_command_line("user@[2001:db8::1]")?;
        assert_eq!(opts.host, "2001:db8::1".into());
        assert_eq!(opts.username, Some("user".to_string()));
        assert_eq!(opts.port, None);

        let opts = SshConnectionOptions::parse_command_line("2001:db8::1")?;
        assert_eq!(opts.host, "2001:db8::1".into());
        assert_eq!(opts.username, None);
        assert_eq!(opts.port, None);

        let opts = SshConnectionOptions::parse_command_line("[2001:db8::1]:2222")?;
        assert_eq!(opts.host, "2001:db8::1".into());
        assert_eq!(opts.username, None);
        assert_eq!(opts.port, Some(2222));

        let opts = SshConnectionOptions::parse_command_line("user@example.com:2222")?;
        assert_eq!(opts.host, "example.com".into());
        assert_eq!(opts.username, Some("user".to_string()));
        assert_eq!(opts.port, Some(2222));

        let opts = SshConnectionOptions::parse_command_line("user@192.168.1.1:2222")?;
        assert_eq!(opts.host, "192.168.1.1".into());
        assert_eq!(opts.username, Some("user".to_string()));
        assert_eq!(opts.port, Some(2222));

        Ok(())
    }

    #[test]
    fn test_parse_port_forward_spec_ipv6() -> Result<()> {
        let pf = parse_port_forward_spec("[::1]:8080:[::1]:80")?;
        assert_eq!(pf.local_host, Some("::1".to_string()));
        assert_eq!(pf.local_port, 8080);
        assert_eq!(pf.remote_host, Some("::1".to_string()));
        assert_eq!(pf.remote_port, 80);

        let pf = parse_port_forward_spec("8080:[::1]:80")?;
        assert_eq!(pf.local_host, None);
        assert_eq!(pf.local_port, 8080);
        assert_eq!(pf.remote_host, Some("::1".to_string()));
        assert_eq!(pf.remote_port, 80);

        let pf = parse_port_forward_spec("[2001:db8::1]:3000:[fe80::1]:4000")?;
        assert_eq!(pf.local_host, Some("2001:db8::1".to_string()));
        assert_eq!(pf.local_port, 3000);
        assert_eq!(pf.remote_host, Some("fe80::1".to_string()));
        assert_eq!(pf.remote_port, 4000);

        let pf = parse_port_forward_spec("127.0.0.1:8080:localhost:80")?;
        assert_eq!(pf.local_host, Some("127.0.0.1".to_string()));
        assert_eq!(pf.local_port, 8080);
        assert_eq!(pf.remote_host, Some("localhost".to_string()));
        assert_eq!(pf.remote_port, 80);

        Ok(())
    }

    #[test]
    fn test_port_forward_ipv6_formatting() {
        let options = SshConnectionOptions {
            host: "example.com".into(),
            port_forwards: Some(vec![SshPortForwardOption {
                local_host: Some("::1".to_string()),
                local_port: 8080,
                remote_host: Some("::1".to_string()),
                remote_port: 80,
            }]),
            ..Default::default()
        };

        let args = options.additional_args();
        assert!(
            args.iter().any(|arg| arg == "-L[::1]:8080:[::1]:80"),
            "expected bracketed IPv6 in -L flag: {args:?}"
        );
    }

    #[test]
    fn test_build_command_with_ipv6_port_forward() -> Result<()> {
        let command = build_command_posix(
            None,
            &[],
            &HashMap::default(),
            None,
            Some((8080, "::1".to_owned(), 80)),
            HashMap::default(),
            PathStyle::Unix,
            "/bin/bash",
            ShellKind::Posix,
            vec![],
            "user@host",
            Interactive::No,
        )?;

        assert!(
            command.args.iter().any(|arg| arg == "8080:[::1]:80"),
            "expected bracketed IPv6 in port forward arg: {:?}",
            command.args
        );

        Ok(())
    }

    /// Guarding the master ssh process changed how it is spawned, so what is
    /// spawned is pinned here: the additional args, then the destination, then
    /// the connection-established shell command, in that order. ssh reads the
    /// destination positionally, so an argument that moves is a connection to
    /// the wrong host.
    #[cfg(windows)]
    #[test]
    fn the_windows_master_command_runs_the_ssh_argv_in_order() {
        let command = MasterProcess::command(
            "askpass.bat".as_ref(),
            r"\\.\pipe\askpass".as_ref(),
            vec!["-p".to_string(), "2222".to_string()],
            "user@host",
        );

        assert_eq!(command.get_program().to_string_lossy(), "ssh");
        let arguments: Vec<String> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            arguments,
            vec![
                "-p".to_string(),
                "2222".to_string(),
                "user@host".to_string(),
                "-t".to_string(),
                format!(
                    "echo '{}'; exec $0",
                    MasterProcess::CONNECTION_ESTABLISHED_MAGIC
                ),
            ]
        );
    }

    /// The ordinary case: nothing in the arguments can name a path, so both
    /// long-lived children move out of Zed's directory and stop pinning it.
    /// The directory they move to has to be outside the checkout, which is the
    /// property that makes moving worth doing at all.
    #[cfg(windows)]
    #[test]
    fn a_long_lived_ssh_child_leaves_the_checkout_when_no_argument_names_a_path() {
        let arguments = vec![
            "-p".to_string(),
            "2222".to_string(),
            "-o".to_string(),
            "ConnectTimeout=10".to_string(),
            "-L8080:localhost:80".to_string(),
            "-o".to_string(),
            "StrictHostKeyChecking=no".to_string(),
            // Absolute paths survive the move, so they are no reason to stay.
            "-i".to_string(),
            r"C:\Users\me\.ssh\id_ed25519".to_string(),
            "-o".to_string(),
            r"UserKnownHostsFile=C:\Users\me\.ssh\known_hosts".to_string(),
            "-F".to_string(),
            "~/.ssh/config".to_string(),
        ];

        let directory = long_lived_child_dir(&arguments).expect("the child should be moved");
        assert_eq!(directory, util::process::stable_child_dir());
        assert!(directory.is_dir(), "{directory:?} is not a directory");

        let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("this crate sits two directories below the repository");
        assert!(
            !directory.starts_with(repository),
            "{directory:?} is inside {repository:?}, so the child would pin it"
        );
    }

    /// A relative option is resolved against the child's own directory, so
    /// moving the child would make ssh read a different file, or none. A pinned
    /// directory is a nuisance; a connection that will not open is not, so
    /// these keep Zed's directory.
    #[cfg(windows)]
    #[test]
    fn a_long_lived_ssh_child_stays_put_when_an_argument_names_a_relative_path() {
        let cases: Vec<Vec<String>> = vec![
            vec!["-i".to_string(), "key".to_string()],
            vec!["-i".to_string(), "keys/id_ed25519".to_string()],
            vec!["-ikeys/id_ed25519".to_string()],
            vec!["-F".to_string(), "ssh_config".to_string()],
            vec!["-Fssh_config".to_string()],
            vec!["-E".to_string(), "logs/ssh.log".to_string()],
            vec!["-o".to_string(), "IdentityFile=key".to_string()],
            vec!["-o".to_string(), "identityfile=./key".to_string()],
            vec!["-oIdentityFile=key".to_string()],
            vec![
                "-o".to_string(),
                "UserKnownHostsFile=hosts/known".to_string(),
            ],
            // A command line cannot be split reliably enough to find the
            // program in it, so any of them keeps the directory.
            vec![
                "-o".to_string(),
                r"ProxyCommand=C:\tools\connect.exe %h %p".to_string(),
            ],
            vec![
                "-o".to_string(),
                "ProxyCommand=./connect.sh %h %p".to_string(),
            ],
            // An option this code has not heard of, whose value still looks
            // like a relative path.
            vec!["-o".to_string(), "SomeNewFile=./secrets/token".to_string()],
            // ssh reads a space the same way it reads the `=`.
            vec!["-o".to_string(), "IdentityFile key".to_string()],
            // A value that reached us apart from the `-o` naming it.
            vec!["./connect.sh %h %p".to_string()],
        ];

        for arguments in cases {
            assert_eq!(
                long_lived_child_dir(&arguments),
                None,
                "{arguments:?} should keep the child in Zed's directory"
            );
        }
    }

    /// The master and the proxy must answer this the same way. Two children of
    /// one connection resolving `-i key` against two different directories is a
    /// connection that authenticates and then cannot start its server.
    #[cfg(windows)]
    #[test]
    fn the_master_and_the_proxy_agree_on_the_directory() {
        for arguments in [
            vec!["-p".to_string(), "2222".to_string()],
            vec!["-i".to_string(), "key".to_string()],
        ] {
            let options = SshConnectionOptions {
                host: "example.com".into(),
                args: Some(arguments.clone()),
                ..Default::default()
            };
            // The master is given `additional_args()` verbatim, and the proxy
            // asks the same options for them.
            assert_eq!(
                long_lived_child_dir(&arguments),
                long_lived_child_dir(&options.additional_args()),
            );
        }
    }

    /// `wait_connected` consumes a line-oriented stdout stream and succeeds
    /// only after the magic marker appears. A local command is enough to test
    /// this seam; no SSH server or network is involved.
    #[cfg(windows)]
    #[test]
    fn the_windows_master_wait_connected_accepts_the_magic_line() {
        let mut process = util::command::new_command("cmd.exe");
        process
            .args([
                "/D",
                "/C",
                "echo prefix && echo ZED_SSH_CONNECTION_ESTABLISHED",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let process = process.spawn().expect("spawning stand-in master");
        let guard = util::process::ProcessTreeGuard::new().expect("creating process guard");
        guard
            .assign_process(process.id())
            .expect("assigning stand-in master");
        let mut master = MasterProcess {
            process,
            _stderr_task: None,
            _guard: guard,
        };

        smol::block_on(master.wait_connected()).expect("magic line should connect");
        smol::block_on(master.process.status()).expect("reaping stand-in master");
    }

    #[cfg(windows)]
    #[test]
    fn the_windows_master_wait_connected_rejects_eof_before_the_magic_line() {
        let mut process = util::command::new_command("cmd.exe");
        process
            .args(["/D", "/C", "echo unrelated; exit /B 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let process = process.spawn().expect("spawning stand-in master");
        let guard = util::process::ProcessTreeGuard::new().expect("creating process guard");
        guard
            .assign_process(process.id())
            .expect("assigning stand-in master");
        let mut master = MasterProcess {
            process,
            _stderr_task: None,
            _guard: guard,
        };

        let error = smol::block_on(master.wait_connected()).expect_err("EOF must fail");
        assert!(
            format!("{error:#}").contains("exited before connection established"),
            "unexpected error: {error:#}"
        );
        smol::block_on(master.process.status()).expect("reaping stand-in master");
    }
}
