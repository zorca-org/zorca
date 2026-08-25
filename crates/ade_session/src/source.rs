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

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};

use crate::deploy::HostExec;
use crate::process::QuietCommand as _;

/// Points at an already-built daemon binary for a matching host platform.
///
/// The counterpart of Zed's `ZED_COPY_REMOTE_SERVER`, and the escape hatch for
/// anyone who builds the daemon their own way.
pub const DAEMON_BINARY_ENV: &str = "ADE_COPY_DAEMON";

/// The cargo package that carries the daemon.
const DAEMON_PACKAGE: &str = "ade_session_daemon";

/// The binary that package produces — not the package name; see that crate's
/// `[[bin]]` stanza.
const DAEMON_BINARY: &str = "ade-daemon";

fn target_dir(cargo_target_dir: Option<&OsStr>, root: &Path) -> PathBuf {
    let base = match cargo_target_dir.filter(|dir| !dir.is_empty()) {
        Some(dir) if Path::new(dir).is_absolute() => PathBuf::from(dir),
        Some(dir) => root.join(dir),
        None => root.join("target"),
    };
    base.join("ade_daemon")
}

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

    /// This process's own platform, without asking anyone — `ade-daemon
    /// --ensure` runs this to report what it is without a second `uname`
    /// hop back to the client. `None` on a platform ADE has no target for.
    pub fn current() -> Option<Self> {
        let os = match std::env::consts::OS {
            "linux" => HostOs::Linux,
            "macos" => HostOs::MacOs,
            _ => return None,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x86_64",
            "aarch64" => "aarch64",
            _ => return None,
        };
        Some(Self { os, arch })
    }

    /// The inverse of [`Self::target_triple`], for a client reading the
    /// `platform=` token back out of an `--ensure` line. Anything this
    /// mapping did not produce parses to `None` rather than a guess.
    pub fn from_target_triple(triple: &str) -> Option<Self> {
        match triple {
            "x86_64-unknown-linux-musl" => Some(Self {
                os: HostOs::Linux,
                arch: "x86_64",
            }),
            "aarch64-unknown-linux-musl" => Some(Self {
                os: HostOs::Linux,
                arch: "aarch64",
            }),
            "x86_64-apple-darwin" => Some(Self {
                os: HostOs::MacOs,
                arch: "x86_64",
            }),
            "aarch64-apple-darwin" => Some(Self {
                os: HostOs::MacOs,
                arch: "aarch64",
            }),
            _ => None,
        }
    }
}

/// The daemon binary to upload to a host of this platform.
///
/// [`DAEMON_BINARY_ENV`] first; otherwise a release build out of this checkout,
/// which on a cold cross-compile takes minutes. Callers run it off the UI
/// thread and it logs its own progress, because there is nothing else to look
/// at while it runs.
pub fn daemon_binary(platform: &HostPlatform) -> Result<Vec<u8>> {
    if let Some(value) = std::env::var_os(DAEMON_BINARY_ENV).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        let bytes = read_binary(&path)?;
        if daemon_binary_matches_platform(&bytes, platform) {
            log::info!("using the {DAEMON_BINARY_ENV} binary at {}", path.display());
            return Ok(bytes);
        }
        log::warn!(
            "ignoring the {DAEMON_BINARY_ENV} binary at {} because it is not for {}; \
             building the correct target instead",
            path.display(),
            platform.target_triple(),
        );
    }

    let path = build_daemon(platform)?;
    let bytes = read_binary(&path)?;
    if !daemon_binary_matches_platform(&bytes, platform) {
        bail!(
            "cargo produced an ade-daemon at {} that is not for {}",
            path.display(),
            platform.target_triple(),
        );
    }
    Ok(bytes)
}

fn read_binary(path: &Path) -> Result<Vec<u8>> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading the ade-daemon binary at {}", path.display()))?;
    log::info!("ade-daemon binary is {} bytes", bytes.len());
    Ok(bytes)
}

/// Whether executable bytes can run on `platform`.
pub fn daemon_binary_matches_platform(bytes: &[u8], platform: &HostPlatform) -> bool {
    binary_platform(bytes).is_some_and(|actual| actual == *platform)
        || fat_mach_o_matches_platform(bytes, platform)
}

fn fat_mach_o_matches_platform(bytes: &[u8], platform: &HostPlatform) -> bool {
    if platform.os != HostOs::MacOs {
        return false;
    }
    let Some(header) = bytes.get(..8) else {
        return false;
    };
    let (entry_size, wide) = match &header[..4] {
        [0xca, 0xfe, 0xba, 0xbe] => (20, false),
        [0xca, 0xfe, 0xba, 0xbf] => (32, true),
        _ => return false,
    };
    let count = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
    let Some(table_len) = count.checked_mul(entry_size) else {
        return false;
    };
    let Some(table_end) = 8usize.checked_add(table_len) else {
        return false;
    };
    let Some(table) = bytes.get(8..table_end) else {
        return false;
    };
    table.chunks_exact(entry_size).any(|entry| {
        let (offset, size) = if wide {
            (
                u64::from_be_bytes(entry[8..16].try_into().unwrap()),
                u64::from_be_bytes(entry[16..24].try_into().unwrap()),
            )
        } else {
            (
                u32::from_be_bytes(entry[8..12].try_into().unwrap()) as u64,
                u32::from_be_bytes(entry[12..16].try_into().unwrap()) as u64,
            )
        };
        let (Ok(offset), Ok(size)) = (usize::try_from(offset), usize::try_from(size)) else {
            return false;
        };
        let Some(end) = offset.checked_add(size) else {
            return false;
        };
        bytes
            .get(offset..end)
            .and_then(binary_platform)
            .is_some_and(|actual| actual == *platform)
    })
}

fn binary_platform(bytes: &[u8]) -> Option<HostPlatform> {
    if bytes.starts_with(b"\x7fELF") {
        return static_elf_platform(bytes);
    }

    let header = bytes.get(..20)?;
    if header[..4] != [0xcf, 0xfa, 0xed, 0xfe] {
        return None;
    }
    let cpu_type = u32::from_le_bytes(header[4..8].try_into().ok()?);
    let arch = match cpu_type {
        0x0100_0007 => "x86_64",
        0x0100_000c => "aarch64",
        _ => return None,
    };
    Some(HostPlatform {
        os: HostOs::MacOs,
        arch,
    })
}

fn static_elf_platform(bytes: &[u8]) -> Option<HostPlatform> {
    const ELF64_HEADER_SIZE: usize = 64;
    const ELF64_PROGRAM_HEADER_SIZE: usize = 56;
    const PT_LOAD: u32 = 1;
    const PT_INTERP: u32 = 3;

    let header = bytes.get(..ELF64_HEADER_SIZE)?;
    if header[4] != 2
        || header[5] != 1
        || !matches!(header[7], 0 | 3)
        || !matches!(u16::from_le_bytes(header[16..18].try_into().ok()?), 2 | 3)
    {
        return None;
    }
    let header_size = u16::from_le_bytes(header[52..54].try_into().ok()?) as usize;
    let table_offset = usize::try_from(u64::from_le_bytes(header[32..40].try_into().ok()?)).ok()?;
    let entry_size = u16::from_le_bytes(header[54..56].try_into().ok()?) as usize;
    let entry_count = u16::from_le_bytes(header[56..58].try_into().ok()?) as usize;
    if header_size < ELF64_HEADER_SIZE
        || table_offset < header_size
        || entry_size < ELF64_PROGRAM_HEADER_SIZE
        || entry_count == 0
    {
        return None;
    }
    let table_len = entry_count.checked_mul(entry_size)?;
    let table = bytes.get(table_offset..table_offset.checked_add(table_len)?)?;
    let mut has_load = false;
    for entry in table.chunks_exact(entry_size) {
        match u32::from_le_bytes(entry[..4].try_into().ok()?) {
            PT_LOAD => has_load = true,
            PT_INTERP => return None,
            _ => {}
        }
    }
    if !has_load {
        return None;
    }

    let arch = match u16::from_le_bytes(header[18..20].try_into().ok()?) {
        62 => "x86_64",
        183 => "aarch64",
        _ => return None,
    };
    Some(HostPlatform {
        os: HostOs::Linux,
        arch,
    })
}

/// Build the daemon for `platform` and answer with where cargo put it.
fn build_daemon(platform: &HostPlatform) -> Result<PathBuf> {
    let triple = platform.target_triple();
    let root = workspace_root();
    let target_dir = target_dir(
        std::env::var_os("CARGO_TARGET_DIR").as_deref(),
        Path::new(root),
    );

    let mut rust_flags = std::env::var("RUSTFLAGS").unwrap_or_default();
    if platform.os == HostOs::Linux {
        // The other half of choosing musl: without this the musl target still
        // links dynamically against musl's own libc.
        rust_flags.push_str(" -C target-feature=+crt-static");
    }

    if platform.is_this_machine() {
        log::info!("building ade-daemon for {triple}");
        run_build(cargo_command(
            root,
            &target_dir,
            "build",
            &triple,
            &rust_flags,
        ))?;
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

        run_build(cargo_command(
            root,
            &target_dir,
            "zigbuild",
            &triple,
            &rust_flags,
        ))?;
    }

    let binary = target_dir
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
    target_dir: &Path,
    subcommand: &str,
    triple: &str,
    rust_flags: &str,
) -> std::process::Command {
    let mut command = std::process::Command::new("cargo");
    command
        .current_dir(root)
        .args([subcommand, "--package", DAEMON_PACKAGE, "--release"])
        .arg("--target-dir")
        .arg(target_dir)
        .args(["--target", triple])
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

    fn elf(machine: u16, interpreter: bool) -> Vec<u8> {
        let entry_count = if interpreter { 2 } else { 1 };
        let mut bytes = vec![0; 64 + 56 * entry_count];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&2u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
        bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&(entry_count as u16).to_le_bytes());
        bytes[64..68].copy_from_slice(&1u32.to_le_bytes());
        if interpreter {
            bytes[120..124].copy_from_slice(&3u32.to_le_bytes());
        }
        bytes
    }

    fn mach_o(cpu_type: u32) -> [u8; 20] {
        let mut bytes = [0; 20];
        bytes[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
        bytes[4..8].copy_from_slice(&cpu_type.to_le_bytes());
        bytes
    }

    fn fat_mach_o(slices: &[[u8; 20]], wide: bool) -> Vec<u8> {
        let entry_size = if wide { 32 } else { 20 };
        let header_len = 8 + entry_size * slices.len();
        let mut bytes = vec![0; header_len];
        bytes[..4].copy_from_slice(if wide {
            &[0xca, 0xfe, 0xba, 0xbf]
        } else {
            &[0xca, 0xfe, 0xba, 0xbe]
        });
        bytes[4..8].copy_from_slice(&(slices.len() as u32).to_be_bytes());
        for (index, slice) in slices.iter().enumerate() {
            let entry = 8 + index * entry_size;
            let offset = header_len + index * slice.len();
            let cpu_type = u32::from_le_bytes(slice[4..8].try_into().unwrap());
            bytes[entry..entry + 4].copy_from_slice(&cpu_type.to_be_bytes());
            if wide {
                bytes[entry + 8..entry + 16].copy_from_slice(&(offset as u64).to_be_bytes());
                bytes[entry + 16..entry + 24].copy_from_slice(&(slice.len() as u64).to_be_bytes());
            } else {
                bytes[entry + 8..entry + 12].copy_from_slice(&(offset as u32).to_be_bytes());
                bytes[entry + 12..entry + 16].copy_from_slice(&(slice.len() as u32).to_be_bytes());
            }
            bytes.extend_from_slice(slice);
        }
        bytes
    }

    #[test]
    fn executable_headers_are_fenced_to_the_host_platform() {
        let linux_x86 = HostPlatform {
            os: HostOs::Linux,
            arch: "x86_64",
        };
        let linux_arm = HostPlatform {
            os: HostOs::Linux,
            arch: "aarch64",
        };
        let mac_x86 = HostPlatform {
            os: HostOs::MacOs,
            arch: "x86_64",
        };
        let mac_arm = HostPlatform {
            os: HostOs::MacOs,
            arch: "aarch64",
        };

        assert!(daemon_binary_matches_platform(&elf(62, false), &linux_x86));
        assert!(daemon_binary_matches_platform(&elf(183, false), &linux_arm));
        assert!(daemon_binary_matches_platform(
            &mach_o(0x0100_0007),
            &mac_x86
        ));
        assert!(daemon_binary_matches_platform(
            &mach_o(0x0100_000c),
            &mac_arm
        ));
        assert!(!daemon_binary_matches_platform(&elf(62, false), &linux_arm));
        assert!(!daemon_binary_matches_platform(&elf(183, false), &mac_arm));
        assert!(!daemon_binary_matches_platform(&elf(62, true), &linux_x86));
        assert!(!daemon_binary_matches_platform(
            b"not executable",
            &linux_x86
        ));

        for universal in [
            fat_mach_o(&[mach_o(0x0100_0007), mach_o(0x0100_000c)], false),
            fat_mach_o(&[mach_o(0x0100_0007), mach_o(0x0100_000c)], true),
        ] {
            assert!(daemon_binary_matches_platform(&universal, &mac_x86));
            assert!(daemon_binary_matches_platform(&universal, &mac_arm));
            assert!(!daemon_binary_matches_platform(&universal, &linux_x86));
        }
    }

    #[test]
    fn daemon_target_dir_follows_cargo_target_dir() {
        let root = Path::new("workspace");
        assert_eq!(target_dir(None, root), root.join("target/ade_daemon"));
        assert_eq!(
            target_dir(Some(OsStr::new("cache")), root),
            root.join("cache/ade_daemon")
        );

        let absolute = std::env::temp_dir().join("cargo-target");
        assert_eq!(
            target_dir(Some(absolute.as_os_str()), root),
            absolute.join("ade_daemon")
        );
    }

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

    #[test]
    fn target_triple_and_from_target_triple_round_trip() {
        for platform in [
            HostPlatform {
                os: HostOs::Linux,
                arch: "x86_64",
            },
            HostPlatform {
                os: HostOs::Linux,
                arch: "aarch64",
            },
            HostPlatform {
                os: HostOs::MacOs,
                arch: "x86_64",
            },
            HostPlatform {
                os: HostOs::MacOs,
                arch: "aarch64",
            },
        ] {
            assert_eq!(
                HostPlatform::from_target_triple(&platform.target_triple()),
                Some(platform)
            );
        }
        assert_eq!(
            HostPlatform::from_target_triple("riscv64-unknown-linux-musl"),
            None
        );
        assert_eq!(
            HostPlatform::from_target_triple("x86_64-fake-unknown-linux-musl"),
            None
        );
        assert_eq!(
            HostPlatform::from_target_triple("x86_64-pc-windows-msvc"),
            None
        );
        assert_eq!(HostPlatform::from_target_triple(""), None);
    }

    /// Whatever `current()` reports for this test machine must be the same
    /// machine `is_this_machine` already recognises — they read the same two
    /// `std::env::consts` values, and must never disagree.
    #[test]
    fn current_agrees_with_is_this_machine() {
        match HostPlatform::current() {
            Some(platform) => assert!(platform.is_this_machine()),
            None => assert!(
                !matches!(std::env::consts::OS, "linux" | "macos")
                    || !matches!(std::env::consts::ARCH, "x86_64" | "aarch64"),
                "current() gave up on a supported platform"
            ),
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
