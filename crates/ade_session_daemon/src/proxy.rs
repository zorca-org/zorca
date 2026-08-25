//! `--stdio-proxy` and `--ensure`: the host-side half of the ssh transport.
//!
//! A remote host is reached with `ssh <host> ~/.ade/bin/ade-daemon
//! --stdio-proxy`. The ssh channel then carries the *same* framed protocol as
//! a local Unix socket does, so one ssh connection serves every session on
//! that host — frames are session-tagged, and multiplexing is already in the
//! protocol.
//!
//! The proxy is deliberately **dumb**: it copies bytes between this process's
//! stdin/stdout and the daemon's socket and never parses a frame. Nothing but
//! protocol bytes may ever reach stdout; diagnostics and logging go to stderr,
//! which ssh keeps separate from the data channel.
//!
//! There are two ways to reach a remote daemon and this module carries both.
//! The proxy pumps the protocol over an ssh *command* channel; [`ensure`] does
//! only the start-if-absent half and exits, for the client that forwards the
//! daemon's socket with `ssh -L` and then speaks to it directly. Either way the
//! policy below is the same code.
//!
//! It owns exactly one piece of policy — **start-if-absent**. If the socket
//! does not answer there is no daemon, so the proxy spawns one (the same
//! binary, `--socket`/`--state-dir` forwarded) and retries the connect. Two
//! proxies racing is fine: the loser's daemon exits through
//! [`Server::bind`](crate::server::Server::bind)'s already-running check, and
//! the retry loop connects to the winner either way.
//!
//! Exit is always 0 and always harmless: stdin EOF (the ssh channel closed) or
//! a closed socket ends the pump, and the daemon keeps every PTY it owns.
//! Detach never kills.

use std::fs::OpenOptions;
use std::net::Shutdown;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use ade_session::client::{Connection, PRE_CUT_DIAGNOSIS};
use ade_session::proto::Hello;
use anyhow::{Context as _, Result, bail};
use smol::Unblock;
use smol::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use smol::net::unix::UnixStream;

use crate::attach::{HANDSHAKE_RETRY_DELAY, handshake_ended_in_eof};
use crate::server::ServerConfig;
use crate::state::create_private_dir;

/// Total time start-if-absent will wait for a freshly spawned daemon to bind.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

/// First retry delay; it doubles up to [`MAX_RETRY_DELAY`].
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(25);

/// Ceiling on the retry backoff, so a slow host still gets several attempts
/// inside [`STARTUP_TIMEOUT`].
const MAX_RETRY_DELAY: Duration = Duration::from_millis(250);

/// Where a proxy-started daemon's stdout and stderr go, inside the state dir.
pub const DAEMON_LOG: &str = "daemon.log";

/// Size of the copy buffer in each direction.
const PIPE_BUFFER: usize = 64 * 1024;

/// Which socket to proxy to, and where a daemon it starts should keep state.
#[derive(Clone, Debug)]
pub struct ProxyConfig {
    pub socket_path: PathBuf,
    pub state_dir: PathBuf,
    /// How long to wait for a daemon started by this proxy to bind.
    pub startup_timeout: Duration,
}

impl ProxyConfig {
    pub fn new(socket_path: impl Into<PathBuf>, state_dir: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            state_dir: state_dir.into(),
            startup_timeout: STARTUP_TIMEOUT,
        }
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self::new(
            ServerConfig::default_socket_path(),
            ServerConfig::default_state_dir(),
        )
    }
}

/// Connect (starting a daemon if there is none), then pump until either end
/// closes.
pub async fn run(config: ProxyConfig) -> Result<()> {
    let socket = connect_or_start(&config).await?;
    pump(socket).await
}

/// Connect to the daemon socket, starting a daemon if nobody answers.
///
/// A failed connect is not distinguished from a missing socket file on
/// purpose: both mean "no daemon is serving here", and a stale socket file is
/// cleaned up by [`Server::bind`](crate::server::Server::bind) anyway.
pub async fn connect_or_start(config: &ProxyConfig) -> Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(&config.socket_path).await {
        return Ok(stream);
    }
    log::info!(
        "no daemon on {}; starting one",
        config.socket_path.display()
    );
    spawn_daemon(config)?;

    let deadline = Instant::now() + config.startup_timeout;
    let mut delay = FIRST_RETRY_DELAY;
    loop {
        sleep(delay).await;
        match UnixStream::connect(&config.socket_path).await {
            Ok(stream) => return Ok(stream),
            Err(err) if Instant::now() >= deadline => bail!(
                "daemon did not come up on {} within {:?}: {err}",
                config.socket_path.display(),
                config.startup_timeout,
            ),
            Err(_) => delay = (delay * 2).min(MAX_RETRY_DELAY),
        }
    }
}

/// `--ensure`: leave a daemon listening on the socket, and say which one.
///
/// The remote half of the ssh transport is a *forwarded socket*, not a proxy
/// process, so something has to make sure a daemon is listening at the far end
/// before the forward is established — nothing can be forwarded to a socket
/// nobody has bound. That is this mode: one short `ssh <host> ade-daemon
/// --ensure` before the one long-lived `ssh -L …`.
///
/// It is [`connect_or_start`] plus a handshake, so it reuses the proxy's
/// start-if-absent policy verbatim, including the racing-proxies case. The
/// handshake is not ceremony: it proves the thing behind the socket really is a
/// daemon speaking this protocol, and its answer is the version line the client
/// gets back.
///
/// Stdout is not a protocol stream here — it is that one line — and the
/// connection is dropped immediately, which is a detach and costs the daemon
/// nothing.
pub async fn ensure(config: ProxyConfig) -> Result<String> {
    // §6.1's one retry, the same rule the attach client follows: a handshake
    // that ends in EOF with no reply is most likely a daemon that predates the
    // protocol cut, but a transient drop looks identical — so the first one
    // buys a second attempt and only the second is diagnosed.
    let mut retried = false;
    let ack = loop {
        let socket = connect_or_start(&config).await?;
        let error = match Connection::new(socket).handshake(Hello::current()).await {
            Ok(ack) => break ack,
            Err(error) => error,
        };
        let pre_cut = handshake_ended_in_eof(&error);
        if pre_cut && !retried {
            retried = true;
            log::debug!("the handshake ended in EOF with no reply; retrying once");
            sleep(HANDSHAKE_RETRY_DELAY).await;
            continue;
        }
        let error = if pre_cut {
            error.context(PRE_CUT_DIAGNOSIS)
        } else {
            error
        };
        return Err(error).with_context(|| {
            format!(
                "handshaking with the daemon on {}",
                config.socket_path.display()
            )
        });
    };
    // Trailing `key=value` tokens, appended so the line stays parseable by a
    // client that only knows the two-token form — and absent entirely from a
    // daemon that predates them, which a client reads as "legacy, leave it
    // alone".
    let mut line = format!("ade-daemon {}", ack.daemon_version);
    if let Some(hash) = &ack.binary_hash {
        line.push_str(&format!(" hash={hash}"));
    }
    if let Some(ready) = ack.upgrade_ready {
        line.push_str(&format!(" upgrade_ready={ready}"));
    }
    // Lets the client validate a prebuilt daemon without another ssh round trip.
    if let Some(platform) = ade_session::HostPlatform::current() {
        line.push_str(&format!(" platform={}", platform.target_triple()));
    }
    // The window this daemon serves, before any handshake has been attempted:
    // a client whose own window is entirely below it must refuse to deploy
    // over these bytes rather than downgrade a daemon it cannot talk to.
    line.push_str(&format!(
        " generations={}..={}",
        ack.min_generation, ack.max_generation
    ));
    Ok(line)
}

/// Park a blocking thread. `smol::Timer` is disallowed by the workspace lints.
async fn sleep(duration: Duration) {
    smol::unblock(move || std::thread::sleep(duration)).await;
}

/// Start a detached daemon from this same binary.
///
/// Detachment is a double fork: the intermediate child `_exit`s immediately so
/// the daemon is reparented to init and can never become this proxy's zombie,
/// and `setsid` puts it in its own session so the ssh channel's SIGHUP cannot
/// reach it. (The daemon also ignores SIGHUP, but not being signalled at all is
/// better than surviving a signal.)
///
/// stdin is `/dev/null` and both output streams go to `<state-dir>/daemon.log`
/// — a daemon writing to the inherited stdout would corrupt the protocol
/// stream of whichever proxy started it.
#[allow(
    clippy::disallowed_methods,
    reason = "starting a detached daemon is \
    inherently a blocking, stdio-configuring spawn; the child is reaped \
    immediately because the double fork makes it exit at once"
)]
fn spawn_daemon(config: &ProxyConfig) -> Result<()> {
    use std::os::unix::process::CommandExt as _;

    let exe = std::env::current_exe().context("locating this binary")?;
    create_private_dir(&config.state_dir)?;
    let log_path = config.state_dir.join(DAEMON_LOG);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;

    let mut command = std::process::Command::new(&exe);
    command
        .arg("--socket")
        .arg(&config.socket_path)
        .arg("--state-dir")
        .arg(&config.state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            log.try_clone().context("cloning the log file")?,
        ))
        .stderr(Stdio::from(log));

    // SAFETY: only `fork`, `_exit` and `setsid` are called between the fork and
    // the exec, all of which are async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            match libc::fork() {
                -1 => return Err(std::io::Error::last_os_error()),
                0 => {}
                _ => libc::_exit(0),
            }
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("spawning {}", exe.display()))?;
    // The intermediate child is already gone; this only collects it.
    child.wait().context("reaping the intermediate process")?;
    Ok(())
}

/// Copy bytes both ways until either side is done.
///
/// The downstream copy (daemon → stdout) is the one that decides when the
/// proxy is finished, because it is the side that observes the daemon closing.
/// The upstream copy runs as a task: when stdin hits EOF it shuts down the
/// socket's write half, the daemon sees a disconnect and drops the connection,
/// and the downstream copy ends — so an ssh channel closing still exits
/// promptly, but only after everything already in flight has been written out.
async fn pump(socket: UnixStream) -> Result<()> {
    let upstream_socket = socket.clone();
    let upstream = smol::spawn(async move {
        let mut stdin = Unblock::new(std::io::stdin());
        let mut to_daemon = upstream_socket.clone();
        let result = pipe(&mut stdin, &mut to_daemon).await;
        // Either way this client is done talking; let the daemon know, so it
        // drops the connection and the downstream copy below can finish.
        let _ = upstream_socket.shutdown(Shutdown::Write);
        if let Err(err) = result {
            log::debug!("stdin pump stopped: {err:#}");
        }
    });

    let mut from_daemon = socket;
    let mut stdout = Unblock::new(std::io::stdout());
    let result = pipe(&mut from_daemon, &mut stdout).await;
    // Cancels the stdin pump; it has nowhere left to write.
    drop(upstream);
    match result {
        // A closed stdout *is* the ssh channel going away, not a failure.
        Err(err) if is_broken_pipe(&err) => Ok(()),
        other => other.context("piping the daemon to stdout"),
    }
}

fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|err| err.kind() == std::io::ErrorKind::BrokenPipe)
}

/// Read-write-flush until EOF.
///
/// Written out rather than delegating to `smol::io::copy` because the flush is
/// load-bearing: stdout is a `LineWriter`, and frame payloads mostly contain no
/// newline, so an unflushed write would sit in its buffer until something
/// eventually pushed a newline through.
async fn pipe<R, W>(reader: &mut R, writer: &mut W) -> Result<()>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut buffer = vec![0u8; PIPE_BUFFER];
    loop {
        let read = reader.read(&mut buffer).await.context("reading")?;
        if read == 0 {
            return Ok(());
        }
        writer.write_all(&buffer[..read]).await.context("writing")?;
        writer.flush().await.context("flushing")?;
    }
}
