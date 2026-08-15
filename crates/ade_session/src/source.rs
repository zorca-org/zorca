//! Where the bytes [`crate::deploy`] uploads come from.
//!
//! [`deploy`](crate::deploy) only moves bytes and deliberately does not care
//! which platform they are for. This module is the other half: it asks a host
//! what it *is*, turns that into a Rust target triple, and produces a daemon
//! binary for it — either one the operator points at, or one built from this
//! checkout.
//!
//! The build flow mirrors Zed's own `build_remote_server_from_source`
//! (`crates/remote/src/transport.rs`) rather than inventing a second one: same
//! env-var override, same triple mapping, same `zig` + `rustup target add` +
//! `cargo-zigbuild` path when the host is not this machine, same
//! `--target-dir` so the cross build never thrashes the main build cache.
//!
//! **Only unix hosts.** A host is a box that binds a Unix socket and owns PTYs;
//! a remote Windows host is out of scope, and [`HostPlatform::parse`] says so
//! rather than guessing a triple for it.
//!
//! **Dev-checkout assumption.** With no [`DAEMON_BINARY_ENV`] override the
//! build runs `cargo` in the directory this crate was compiled from. That is
//! true of every build the fork currently produces; a shipped binary would need
//! the override (or a bundled daemon), and would otherwise fail with cargo's
//! own "no such directory" rather than anything subtler.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use crate::deploy::HostExec;
use crate::process::QuietCommand as _;

/// Points at an already-built daemon binary, skipping the build entirely.
///
/// The counterpart of Zed's `ZED_COPY_REMOTE_SERVER`, and the escape hatch for
/// anyone who builds the daemon their own way.
pub const DAEMON_BINARY_ENV: &str = "ADE_COPY_DAEMON";

/// The cargo package that carries the daemon.
const DAEMON_PACKAGE: &str = "ade_session_daemon";

/// The binary that package produces — not the package name; see that crate's
/// `[[bin]]` stanza.
const DAEMON_BINARY: &str = "ade-daemon";

/// A build directory of its own, so a cross build for a host never invalidates
/// the artifacts of the Zed build running beside it.
const TARGET_DIR: &str = "target/ade_daemon";

/// Deployed binaries are release builds: they are uploaded once and then run
/// for as long as the host is up.
const PROFILE_DIR: &str = "release";

/// The operating system half of a host's platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostOs {
    Linux,
    MacOs,
}

impl HostOs {
    /// The name `std::env::consts::OS` uses, so "is this host this machine?" is
    /// one comparison.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::MacOs => "macos",
        }
    }
}

/// What a host is, in the terms a Rust target triple is built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostPlatform {
    pub os: HostOs,
    /// Normalised to the name Rust uses, so `arm64` from a Darwin `uname`
    /// arrives here as `aarch64`.
    pub arch: &'static str,
}

impl HostPlatform {
    /// Ask the host what it is: one `uname -sm` over whatever transport the
    /// caller already has.
    pub fn probe(host: &dyn HostExec) -> Result<Self> {
        let output = host.run(&["uname".to_owned(), "-sm".to_owned()])?;
        if !output.success() {
            bail!(
                "could not read the host's platform: `uname -sm` exited {}: {}. \
                 ade-daemon deployment targets unix hosts",
                output.exit_code,
                output.stderr.trim(),
            );
        }
        let platform = Self::parse(&output.stdout)?;
        log::debug!("host platform is {platform:?}");
        Ok(platform)
    }

    /// Parse `uname -sm` output — the kernel name and the machine, in that
    /// order.
    ///
    /// Strict: anything that is not a unix ADE has a target for is an error,
    /// never a guess. Deploying the wrong triple produces a binary that fails
    /// at `--version` on the far side, which reads exactly like "not
    /// installed" and would loop.
    pub fn parse(uname: &str) -> Result<Self> {
        let mut fields = uname.split_whitespace();
        let (Some(kernel), Some(machine)) = (fields.next(), fields.next()) else {
            bail!("could not read `uname -sm` output {uname:?}");
        };
        let os = match kernel {
            "Linux" => HostOs::Linux,
            "Darwin" => HostOs::MacOs,
            other => bail!(
                "ade-daemon deployment targets unix hosts; this host reports {other:?}, \
                 so build and install ade-daemon there by hand"
            ),
        };
        // Both spellings of each arch: Linux says `x86_64`/`aarch64`, Darwin
        // says `arm64`, and a few BSD-descended `uname`s say `amd64`.
        let arch = match machine {
            "x86_64" | "amd64" => "x86_64",
            "aarch64" | "arm64" => "aarch64",
            other => bail!("there is no ade-daemon target for the host architecture {other:?}"),
        };
        Ok(Self { os, arch })
    }

    /// The Rust target triple to build for this host.
    ///
    /// Linux gets musl, and therefore a statically linked binary: the daemon is
    /// uploaded to a host whose glibc is unknown and frequently older than this
    /// machine's, and a dynamic link would fail at exec with a version error.
    pub fn target_triple(&self) -> String {
        match self.os {
            HostOs::Linux => format!("{}-unknown-linux-musl", self.arch),
            HostOs::MacOs => format!("{}-apple-darwin", self.arch),
        }
    }

    /// Can this machine's toolchain build for the host without cross-compiling
    /// through zig?
    ///
    /// Arch and OS only, exactly as Zed's remote-server build decides it: a
    /// glibc Linux building the musl target still counts, because that is a
    /// target the local `cc` handles once `rustup target add` has run.
    pub fn is_this_machine(&self) -> bool {
        self.arch == std::env::consts::ARCH && self.os.as_str() == std::env::consts::OS
    }
}

/// The daemon binary to upload to a host of this platform.
///
/// [`DAEMON_BINARY_ENV`] first; otherwise a release build out of this checkout,
/// which on a cold cross-compile takes minutes. Callers run it off the UI
/// thread and it logs its own progress, because there is nothing else to look
/// at while it runs.
pub fn daemon_binary(platform: &HostPlatform) -> Result<Vec<u8>> {
    let path = match std::env::var_os(DAEMON_BINARY_ENV) {
        Some(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            log::info!("using the {DAEMON_BINARY_ENV} binary at {}", path.display());
            path
        }
        _ => build_daemon(platform)?,
    };
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading the ade-daemon binary at {}", path.display()))?;
    log::info!("ade-daemon binary is {} bytes", bytes.len());
    Ok(bytes)
}

/// Build the daemon for `platform` and answer with where cargo put it.
fn build_daemon(platform: &HostPlatform) -> Result<PathBuf> {
    let triple = platform.target_triple();
    let root = workspace_root();

    let mut rust_flags = std::env::var("RUSTFLAGS").unwrap_or_default();
    if platform.os == HostOs::Linux {
        // The other half of choosing musl: without this the musl target still
        // links dynamically against musl's own libc.
        rust_flags.push_str(" -C target-feature=+crt-static");
    }

    if platform.is_this_machine() {
        log::info!("building ade-daemon for {triple}");
        run_build(cargo_command(root, "build", &triple, &rust_flags))?;
    } else {
        log::info!("cross-building ade-daemon for {triple} with zig");
        if which::which("zig").is_err() {
            bail!(
                "zig is not on $PATH, and building ade-daemon for {triple} from this \
                 machine needs it (https://ziglang.org/learn/getting-started). \
                 Alternatively point {DAEMON_BINARY_ENV} at a binary you built yourself."
            );
        }
        let rustup = which::which("rustup")
            .context("rustup is not on $PATH, install rustup (https://rustup.rs/)")?;

        log::info!("adding the rustup target {triple}");
        let mut add_target = std::process::Command::new(rustup);
        add_target
            .current_dir(root)
            .args(["target", "add", &triple]);
        run_build(add_target)?;

        if which::which("cargo-zigbuild").is_err() {
            log::info!("installing cargo-zigbuild");
            let mut install = std::process::Command::new("cargo");
            install.args(["install", "--locked", "cargo-zigbuild"]);
            run_build(install)?;
        }

        run_build(cargo_command(root, "zigbuild", &triple, &rust_flags))?;
    }

    let binary = Path::new(root)
        .join(TARGET_DIR)
        .join(&triple)
        .join(PROFILE_DIR)
        .join(DAEMON_BINARY);
    if !binary.is_file() {
        bail!(
            "cargo reported success but there is no ade-daemon at {}",
            binary.display()
        );
    }
    log::info!("built ade-daemon at {}", binary.display());
    Ok(binary)
}

/// `cargo <subcommand> -p ade_session_daemon --release --target <triple>`, in
/// its own target directory. `build` and `zigbuild` take identical arguments,
/// which is what makes the cross path a one-word change.
fn cargo_command(
    root: &'static str,
    subcommand: &str,
    triple: &str,
    rust_flags: &str,
) -> std::process::Command {
    let mut command = std::process::Command::new("cargo");
    command
        .current_dir(root)
        .args([
            subcommand,
            "--package",
            DAEMON_PACKAGE,
            "--release",
            "--target-dir",
            TARGET_DIR,
            "--target",
            triple,
        ])
        .env("RUSTFLAGS", rust_flags);
    command
}

/// Run a build step, keeping its output for the error message rather than
/// letting it scroll past on a stream nobody is watching.
#[allow(
    clippy::disallowed_methods,
    reason = "deployment is a blocking, strictly sequential operation by design; \
    see the HostExec docs"
)]
fn run_build(mut command: std::process::Command) -> Result<()> {
    let described = format!("{command:?}");
    let output = command
        .stdin(std::process::Stdio::null())
        .quiet()
        .output()
        .with_context(|| format!("running {described}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::error!("{described} failed:\n{stderr}");
        // Only the tail: a cargo log is thousands of lines and the error is at
        // the end of it. The whole thing is in the log line above.
        bail!("{described} failed:\n{}", tail_chars(&stderr, 4000));
    }
    Ok(())
}

/// Last `max` chars of `text`, sliced on a char boundary so a truncated cargo
/// log stays valid UTF-8.
fn tail_chars(text: &str, max: usize) -> String {
    let mut tail: Vec<char> = text.chars().rev().take(max).collect();
    tail.reverse();
    tail.into_iter().collect()
}

/// The checkout this crate was compiled from — `crates/ade_session/../..`.
const fn workspace_root() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uname_output_becomes_a_target_triple() {
        assert_eq!(
            HostPlatform::parse("Linux x86_64\n").expect("a linux host"),
            HostPlatform {
                os: HostOs::Linux,
                arch: "x86_64"
            }
        );
        assert_eq!(
            HostPlatform::parse("Linux x86_64")
                .expect("a linux host")
                .target_triple(),
            "x86_64-unknown-linux-musl"
        );
        assert_eq!(
            HostPlatform::parse("Linux aarch64")
                .expect("an arm linux host")
                .target_triple(),
            "aarch64-unknown-linux-musl"
        );
        // Darwin spells the same architecture differently, and the triple must
        // not.
        assert_eq!(
            HostPlatform::parse("Darwin arm64")
                .expect("an apple silicon host")
                .target_triple(),
            "aarch64-apple-darwin"
        );
        assert_eq!(
            HostPlatform::parse("Darwin x86_64")
                .expect("an intel mac")
                .target_triple(),
            "x86_64-apple-darwin"
        );
    }

    /// A remote Windows host is out of scope, and the error has to say that
    /// rather than produce a triple nothing on the far side can run.
    #[test]
    fn a_non_unix_host_is_refused_by_name() {
        for uname in [
            "MINGW64_NT-10.0-26100 x86_64",
            "CYGWIN_NT-10.0 x86_64",
            "FreeBSD amd64",
        ] {
            let message = HostPlatform::parse(uname)
                .expect_err("not a supported host")
                .to_string();
            assert!(
                message.contains("targets unix hosts"),
                "unhelpful error for {uname:?}: {message}"
            );
        }
    }

    #[test]
    fn an_unknown_architecture_is_refused_rather_than_guessed() {
        let message = HostPlatform::parse("Linux riscv64")
            .expect_err("no target for riscv")
            .to_string();
        assert!(message.contains("riscv64"), "unhelpful error: {message}");
    }

    #[test]
    fn empty_or_partial_uname_output_is_an_error() {
        for uname in ["", "\n", "Linux"] {
            assert!(
                HostPlatform::parse(uname).is_err(),
                "{uname:?} is not a platform"
            );
        }
    }

    /// The one comparison the build path branches on, checked against the
    /// machine actually running the test.
    #[test]
    fn this_machine_is_recognised_as_itself() {
        let here = match std::env::consts::OS {
            "linux" => Some(HostOs::Linux),
            "macos" => Some(HostOs::MacOs),
            _ => None,
        };
        let Some(os) = here else {
            // Windows: every host is a cross build from here, which is the
            // branch this assertion would otherwise not reach.
            assert!(
                !HostPlatform {
                    os: HostOs::Linux,
                    arch: "x86_64"
                }
                .is_this_machine()
            );
            return;
        };
        let arch = match std::env::consts::ARCH {
            "aarch64" => "aarch64",
            _ => "x86_64",
        };
        assert!(HostPlatform { os, arch }.is_this_machine());
        assert!(
            !HostPlatform {
                os,
                arch: if arch == "x86_64" {
                    "aarch64"
                } else {
                    "x86_64"
                }
            }
            .is_this_machine()
        );
    }
}
