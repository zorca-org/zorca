//! `ade-daemon` — one per host, owns PTYs, outlives its clients.
//!
//! Four modes, one binary. With no mode argument this *is* the daemon: it
//! binds the socket and serves. With `--stdio-proxy` it is the thin byte pump
//! that `ssh <host> ~/.ade/bin/ade-daemon --stdio-proxy` runs at the far end,
//! which starts a daemon if none is listening. With `--ensure` it does only the
//! start-if-absent half of that and exits, which is what a client runs over ssh
//! before forwarding the socket. With `attach <session-id>` it is the
//! interactive terminal client that a Zed terminal runs, the way it used to run
//! `tmux attach`. One binary because deployment then has exactly one file to
//! upload.
//!
//! Three of those four modes are unix-only, because they *are* the daemon: it
//! binds a Unix socket and owns PTYs on a host, and a host is a unix box for
//! now. `attach` is not — it only speaks to one over a socket or a port, so it
//! builds on Windows too, where Zed runs it in a ConPTY against the local end
//! of a TCP-mode forward. Hence two `main`s: the same parser would have to
//! reject most of its own flags on Windows, and saying so once per mode reads
//! worse than a parser that only knows the mode Windows has.
//!
//! All logic lives in the library half of this crate; `main` is argument
//! parsing plus a call into it.

/// Exactly one mode per run.
///
/// Branch order must not be the arbiter: `--stdio-proxy --ensure` would
/// silently run ensure, `attach <id> --ensure` would silently attach. Refused
/// by name here, the same way `--socket`/`--tcp` already are. No mode at all
/// is the fourth mode — the daemon itself — and cannot conflict.
#[cfg(unix)]
fn one_mode(attach: bool, stdio_proxy: bool, ensure: bool) -> anyhow::Result<()> {
    let given: Vec<&str> = [
        ("attach", attach),
        ("--stdio-proxy", stdio_proxy),
        ("--ensure", ensure),
    ]
    .into_iter()
    .filter(|(_, given)| *given)
    .map(|(mode, _)| mode)
    .collect();
    if given.len() > 1 {
        anyhow::bail!(
            "{} are different modes: give exactly one\n\n{USAGE}",
            given.join(" and ")
        );
    }
    Ok(())
}

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    use ade_session_daemon::{
        AttachConfig, DAEMON_VERSION, ProxyConfig, Server, ServerConfig, attach, proxy,
    };
    use anyhow::bail;

    let mut config = ServerConfig::default();
    let mut stdio_proxy = false;
    let mut ensure = false;
    let mut attach_to = None;
    // Tracked rather than read off `config`, because `--socket` has a default:
    // "exactly one address" can only be checked against what was *given*.
    let mut socket_given = false;
    let mut tcp = None;
    let mut view_id = None;
    let mut expected_daemon_id = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("ade-daemon {DAEMON_VERSION}");
                return Ok(());
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--stdio-proxy" => stdio_proxy = true,
            "--ensure" => ensure = true,
            "attach" => match args.next() {
                Some(session_id) => attach_to = Some(session_id),
                None => bail!("attach needs a session id\n\n{USAGE}"),
            },
            "--socket" => match args.next() {
                Some(path) => {
                    config.socket_path = path.into();
                    socket_given = true;
                }
                None => bail!("--socket needs a path"),
            },
            "--tcp" => match args.next() {
                Some(address) => tcp = Some(address),
                None => bail!("--tcp needs an address, e.g. 127.0.0.1:7654"),
            },
            "--state-dir" => match args.next() {
                Some(path) => config.state_dir = path.into(),
                None => bail!("--state-dir needs a path"),
            },
            "--view-id" => match args.next() {
                Some(id) => view_id = Some(id),
                None => bail!("--view-id needs an id"),
            },
            "--expected-daemon-id" => match args.next() {
                Some(id) => expected_daemon_id = Some(id),
                None => bail!("--expected-daemon-id needs an id"),
            },
            other => bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }

    one_mode(attach_to.is_some(), stdio_proxy, ensure)?;

    // `env_logger` writes to stderr, which is what makes it safe in proxy mode:
    // stdout carries protocol frames and nothing else may ever touch it. In
    // attach mode stdout is the session's own output, and the same rule holds.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    if let Some(session_id) = attach_to {
        // Exactly one address. Both is a caller that does not know which
        // transport it is on, and guessing for it would silently attach to
        // whichever daemon happened to answer.
        if socket_given && tcp.is_some() {
            bail!("attach takes --socket or --tcp, not both\n\n{USAGE}");
        }
        let attach_config = match tcp {
            Some(address) => AttachConfig::tcp(address, session_id),
            None => AttachConfig::new(config.socket_path, session_id),
        }
        .with_view_id(view_id)
        .with_expected_daemon_id(expected_daemon_id);
        return smol::block_on(attach::run(attach_config));
    }

    // Every other mode listens on or proxies to a socket the daemon binds
    // itself; a loopback address only ever names the local end of a forward.
    if tcp.is_some() {
        bail!("--tcp is only for attach\n\n{USAGE}");
    }
    // A daemon does not check its own identity, and a proxy carries whatever
    // the client behind it handshakes with.
    if expected_daemon_id.is_some() {
        bail!("--expected-daemon-id is only for attach\n\n{USAGE}");
    }

    if ensure {
        let proxy_config = ProxyConfig::new(config.socket_path, config.state_dir);
        // The one line on stdout is a report, not a protocol stream: `--ensure`
        // exits as soon as a daemon is known to be listening.
        println!("{}", smol::block_on(proxy::ensure(proxy_config))?);
        return Ok(());
    }

    if stdio_proxy {
        let proxy_config = ProxyConfig::new(config.socket_path, config.state_dir);
        return smol::block_on(proxy::run(proxy_config));
    }

    // A dying ssh channel sends SIGHUP to whatever it spawned. The daemon must
    // survive that with its PTYs intact: sessions die only on an explicit Kill.
    // SAFETY: `signal` with SIG_IGN is async-signal-safe and this runs before
    // any thread is spawned.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }

    let server = Server::bind(config)?;
    smol::block_on(server.run())
}

#[cfg(unix)]
const USAGE: &str = "\
ade-daemon — the ADE per-host session daemon

Usage: ade-daemon [--socket <path>] [--state-dir <dir>]
       ade-daemon --stdio-proxy [--socket <path>] [--state-dir <dir>]
       ade-daemon --ensure [--socket <path>] [--state-dir <dir>]
       ade-daemon attach <session-id> [--socket <path> | --tcp <address>]
                                      [--view-id <id>]
                                      [--expected-daemon-id <id>]

Options:
      --stdio-proxy       Pipe stdin/stdout to the daemon socket, starting a
                          daemon first if nothing is listening. This is what
                          ssh runs on a remote host: stdout carries protocol
                          frames only, diagnostics go to stderr.
      --ensure            Make sure a daemon is listening on the socket,
                          starting one if none is, then print its version and
                          exit. Run over ssh before forwarding the socket.
      attach <id>         Attach this terminal to a session: its output on
                          stdout, this tty's input written to it. Never starts
                          a daemon, and exiting only detaches.
      --socket <path>     Unix socket to listen on / proxy to / attach through
                          (default: $XDG_RUNTIME_DIR/ade/daemon.sock,
                           else ~/.ade/daemon.sock)
      --tcp <address>     Attach through a loopback address instead, e.g.
                          127.0.0.1:7654 — the local end of an `ssh -L` forward
                          on a client whose ssh cannot bind a Unix socket.
                          Attach only, and never together with --socket.
      --view-id <id>      Which terminal view this attach draws, so the daemon
                          can hold the pty at this view's size while it has
                          focus. Attach only.
      --expected-daemon-id <id>
                          Which daemon instance this terminal is for. If the
                          daemon answering is another one, or reports no id,
                          the attach fails instead of reaching a stranger's
                          session. Attach only.
      --state-dir <dir>   Where sessions.json lives (default: ~/.ade/daemon)
  -V, --version           Print the version and exit
  -h, --help              Print this help and exit";

/// The Windows binary is the attach client and nothing else.
///
/// It parses only the argv the app actually builds here —
/// `ade-daemon attach <id> --tcp <address>` — and refuses the daemon's own
/// modes with the reason, at argument level rather than by not existing, so a
/// stale command line gets an error and not a silent no-op.
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use ade_session_daemon::{AttachConfig, DAEMON_VERSION, attach};
    use anyhow::bail;

    let mut attach_to = None;
    let mut tcp = None;
    let mut view_id = None;
    let mut expected_daemon_id = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("ade-daemon {DAEMON_VERSION}");
                return Ok(());
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            "attach" => match args.next() {
                Some(session_id) => attach_to = Some(session_id),
                None => bail!("attach needs a session id\n\n{USAGE}"),
            },
            "--tcp" => match args.next() {
                Some(address) => tcp = Some(address),
                None => bail!("--tcp needs an address, e.g. 127.0.0.1:7654"),
            },
            "--view-id" => match args.next() {
                Some(id) => view_id = Some(id),
                None => bail!("--view-id needs an id"),
            },
            "--expected-daemon-id" => match args.next() {
                Some(id) => expected_daemon_id = Some(id),
                None => bail!("--expected-daemon-id needs an id"),
            },
            // Not "unsupported flag": there is no Unix socket to name on this
            // client, which is the whole reason `--tcp` exists.
            "--socket" => bail!(
                "--socket is not available on Windows: attach with --tcp <address> instead\n\n{USAGE}"
            ),
            "--stdio-proxy" | "--ensure" | "--state-dir" => {
                bail!("{arg} is not available on Windows: only `attach` runs here\n\n{USAGE}")
            }
            other => bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }

    // stderr, because stdout is the session's own output. Same rule as unix.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let Some(session_id) = attach_to else {
        bail!("only `ade-daemon attach` runs on Windows\n\n{USAGE}");
    };
    let Some(address) = tcp else {
        bail!("attach needs --tcp <address> on Windows\n\n{USAGE}");
    };
    smol::block_on(attach::run(
        AttachConfig::tcp(address, session_id)
            .with_view_id(view_id)
            .with_expected_daemon_id(expected_daemon_id),
    ))
}

#[cfg(windows)]
const USAGE: &str = "\
ade-daemon — the ADE per-host session daemon (Windows: attach only)

Usage: ade-daemon attach <session-id> --tcp <address> [--view-id <id>]
                                                      [--expected-daemon-id <id>]

Options:
      attach <id>         Attach this terminal to a session on a host's daemon:
                          its output on stdout, this console's input written to
                          it. Never starts a daemon, and exiting only detaches.
      --tcp <address>     Where the daemon is reachable, e.g. 127.0.0.1:7654 —
                          the local end of an `ssh -L` forward. Required here:
                          Windows ssh cannot bind a Unix socket, so --socket is
                          not available.
      --view-id <id>      Which terminal view this attach draws, so the daemon
                          can hold the pty at this view's size while it has
                          focus.
      --expected-daemon-id <id>
                          Which daemon instance this terminal is for. If the
                          daemon answering is another one, or reports no id,
                          the attach fails instead of reaching a stranger's
                          session.
  -V, --version           Print the version and exit
  -h, --help              Print this help and exit

The daemon itself (no mode, --stdio-proxy, --ensure) runs on unix hosts only.";

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The answer names the conflict, not whichever branch this file happens
    /// to test first.
    #[test]
    fn two_modes_at_once_are_refused_by_name() {
        for (attach, stdio_proxy, ensure, named) in [
            (true, true, false, "attach and --stdio-proxy"),
            (true, false, true, "attach and --ensure"),
            (false, true, true, "--stdio-proxy and --ensure"),
            (true, true, true, "attach and --stdio-proxy and --ensure"),
        ] {
            let error = one_mode(attach, stdio_proxy, ensure)
                .err()
                .unwrap_or_else(|| panic!("{named} are two modes and must be refused"));
            assert!(
                error.to_string().contains(named),
                "the refusal does not name the conflict: {error:#}"
            );
        }
    }

    /// And every invocation that was legal before still is — including no mode
    /// at all, which is the daemon itself.
    #[test]
    fn one_mode_at_a_time_is_still_legal() {
        for (attach, stdio_proxy, ensure) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            one_mode(attach, stdio_proxy, ensure).expect("exactly one mode is what is allowed");
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn main() {
    eprintln!(
        "ade_session_daemon {} does not support this platform yet: the v1 \
         transport is a Unix socket.",
        ade_session_daemon::DAEMON_VERSION
    );
    std::process::exit(1);
}
