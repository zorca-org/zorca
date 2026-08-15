//! The one thing every child ADE spawns has in common: on Windows it must not
//! open a console window.
//!
//! A GUI process on Windows has no console, so `CreateProcess` gives one to any
//! console subsystem child it starts — `ssh.exe`, `ade-daemon.exe`, `tmux` —
//! and the user sees a window flash open and shut. A spawn that *retries*
//! (the status stream's re-subscribe loop) flashes one per attempt, which is
//! how this was found.
//!
//! [`CREATE_NO_WINDOW`] suppresses that, and [`QuietCommand::quiet`] is how it
//! is applied. Every spawn in this crate and in `ade_workspaces` goes through
//! it, so "did this site remember the flag?" is one grep rather than a review.
//!
//! It is deliberately **not** applied to the attach client: that one is spawned
//! by Zed's terminal, wants the console it is given, and is not ours to
//! configure from here.
//!
//! On unix the whole thing is an identity function — the trait still exists so
//! that callers never need a `cfg` of their own.

/// The `CreateProcess` flag that gives a console-subsystem child no console at
/// all, rather than a new window.
///
/// Spelled out rather than pulled from a `windows-sys` binding because this
/// crate is a leaf library with no platform dependencies, and the value is
/// fixed ABI: [process creation flags][1].
///
/// [1]: https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Spawn without a console window.
///
/// Implemented for both command builders ADE uses — `std::process::Command` for
/// the blocking sites and `async_process::Command` for the framed transport —
/// because the two are unrelated types with the same problem.
pub trait QuietCommand {
    /// Suppress the console window this child would otherwise get on Windows.
    /// A no-op everywhere else.
    ///
    /// Returns `&mut Self` so it drops into an existing builder chain.
    fn quiet(&mut self) -> &mut Self;
}

impl QuietCommand for std::process::Command {
    #[cfg(windows)]
    fn quiet(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt as _;

        self.creation_flags(CREATE_NO_WINDOW)
    }

    #[cfg(not(windows))]
    fn quiet(&mut self) -> &mut Self {
        self
    }
}

impl QuietCommand for async_process::Command {
    #[cfg(windows)]
    fn quiet(&mut self) -> &mut Self {
        use async_process::windows::CommandExt as _;

        self.creation_flags(CREATE_NO_WINDOW)
    }

    #[cfg(not(windows))]
    fn quiet(&mut self) -> &mut Self {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flag is the *only* thing this adds: the program and its arguments
    /// come back untouched, and what comes back is the same builder, so the
    /// call drops into a chain without reordering anything after it.
    ///
    /// Nothing here spawns. Whether `CREATE_NO_WINDOW` reaches `CreateProcess`
    /// is a Windows fact that no Linux run can observe, and it is pinned by
    /// cross-compiling to `x86_64-pc-windows-msvc` instead.
    #[test]
    fn quiet_changes_nothing_a_caller_can_see() {
        let mut command = std::process::Command::new("ssh");
        command.args(["-N", "host"]);

        let quieted = command.quiet();
        assert_eq!(quieted.get_program(), "ssh");
        assert_eq!(quieted.get_args().collect::<Vec<_>>(), ["-N", "host"]);
    }

    /// The same call on the other builder — a wholly unrelated type behind the
    /// same one-word method, which is the point of the trait.
    #[test]
    fn the_async_builder_takes_the_same_call() {
        let mut command = async_process::Command::new("ade-daemon");
        command.arg("--stdio-proxy");

        // `async_process::Command` has no getters, so its own `Debug` is what
        // there is to look at; it prints the program and arguments.
        let described = format!("{:?}", command.quiet());
        assert!(described.contains("ade-daemon"), "{described}");
        assert!(described.contains("--stdio-proxy"), "{described}");
    }
}
