use anyhow::{Context as _, Result};
use std::path::PathBuf;
use std::process::Stdio;

#[cfg(windows)]
pub use windows_process_tree::ProcessTreeGuard;

/// A directory that a long-lived child process can sit in for as long as it
/// runs.
///
/// A child inherits its parent's current directory, and on Windows that
/// directory is held open for the life of the child: nothing can rename or
/// delete a directory a process is sitting in. A child that outlives the
/// operation which started it — an ssh forward, a daemon — therefore pins
/// whatever directory the application was launched from, which for a
/// development build is a checkout or a git worktree the user then cannot
/// remove.
///
/// The user's home directory is the answer whenever there is one: it is
/// outside every checkout, and it lasts longer than any child. When there is
/// no usable home directory, the documented fallback is
/// [`std::env::temp_dir`] — likewise never inside a repository, and the one
/// directory every platform is guaranteed to have.
pub fn stable_child_dir() -> PathBuf {
    // Existence is part of "available": a `HOME` pointing at a directory that
    // is not there makes every spawn fail rather than merely start elsewhere.
    dirs::home_dir()
        .filter(|home| home.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

/// A wrapper around `smol::process::Child` that ensures all subprocesses
/// are killed when the process is terminated: on Unix by using process
/// groups, and on Windows by using job objects.
///
/// On Windows, dropping this struct closes the job object handle, which
/// terminates all processes in the job. This also applies when the Zed
/// process exits for any reason (including crashes), since the OS closes
/// its handles, so spawned process trees can never outlive Zed.
pub struct Child {
    process: smol::process::Child,
    #[cfg(windows)]
    guard: Option<ProcessTreeGuard>,
}

impl std::ops::Deref for Child {
    type Target = smol::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.process
    }
}

impl std::ops::DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.process
    }
}

impl Child {
    #[cfg(not(windows))]
    pub fn spawn(
        mut command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Self> {
        crate::set_pre_exec_to_start_new_session(&mut command);
        let mut command = smol::process::Command::from(command);
        let process = command
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn command {}",
                    crate::redact::redact_command(&format!("{command:?}"))
                )
            })?;
        Ok(Self { process })
    }

    #[cfg(windows)]
    pub fn spawn(
        command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Self> {
        let mut command = smol::process::Command::from(command);
        let process = command
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn command {}",
                    crate::redact::redact_command(&format!("{command:?}"))
                )
            })?;

        // Guard the child so that its whole process tree (e.g. node workers
        // and MCP servers spawned by agent servers) is reaped even if the
        // direct child doesn't clean it up. If the guard can't be set up, the
        // child is still owned by `self` and `kill` falls back to terminating
        // just the direct child.
        let guard = ProcessTreeGuard::new()
            .and_then(|guard| {
                guard.assign_process(process.id())?;
                Ok(guard)
            })
            .map_err(|error| {
                log::error!("failed to assign spawned process to a job object: {error:#}");
            })
            .ok();

        Ok(Self { process, guard })
    }

    /// Consumes the child, draining its stdout/stderr and waiting for it to
    /// exit, then returns the collected output.
    pub async fn output(self) -> Result<std::process::Output> {
        // NOTE: Keep `self` alive across this await, do not destructure it to
        // pull `process` out first. On Windows that drops the guard early,
        // which triggers `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and kills the
        // child before `output()` finishes collecting its stdout/stderr.
        Ok(self.process.output().await?)
    }

    #[cfg(not(windows))]
    pub fn kill(&mut self) -> Result<()> {
        let pid = self.process.id();
        unsafe {
            libc::killpg(pid as i32, libc::SIGKILL);
        }
        Ok(())
    }

    #[cfg(windows)]
    pub fn kill(&mut self) -> Result<()> {
        if let Some(guard) = &self.guard {
            guard.terminate()
        } else {
            self.process.kill()?;
            Ok(())
        }
    }
}

#[cfg(windows)]
mod windows_process_tree {
    use crate::ResultExt as _;
    use anyhow::{Context as _, Result};
    use windows::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject, TerminateJobObject,
            },
            Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
        },
    };

    /// An RAII guard over a Win32 job object configured with
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: all processes assigned to the job
    /// (and their descendants) are terminated when the last handle to the job
    /// is closed, which happens when this struct is dropped, or when the OS
    /// closes the owning process's handles after it exits for any reason. A
    /// process tree guarded this way can therefore never outlive the owning
    /// application process.
    ///
    /// The guard owns the job object, not the child. Callers keep their
    /// `std::process::Child` or `async_process::Child` and remain responsible
    /// for waiting on it; the guard only bounds the lifetime of the process
    /// tree. Hold the guard for at least as long as the child is wanted alive,
    /// since dropping it kills the tree.
    #[derive(Debug)]
    pub struct ProcessTreeGuard(HANDLE);

    // SAFETY: Job object handles can be used from any thread.
    unsafe impl Send for ProcessTreeGuard {}
    unsafe impl Sync for ProcessTreeGuard {}

    impl ProcessTreeGuard {
        pub fn new() -> Result<Self> {
            unsafe {
                let job =
                    Self(CreateJobObjectW(None, None).context("failed to create job object")?);
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
                .context("failed to set job object limits")?;
                Ok(job)
            }
        }

        /// Assigns an already running process to the job. Every process it
        /// spawns afterwards joins the job too.
        ///
        /// # The caller must own `pid`, and must still own it here
        ///
        /// A pid is not a handle. Windows reuses a pid as soon as the process
        /// it named has exited *and* every handle to it has been closed, so a
        /// pid whose process the caller has already reaped — or never owned —
        /// can name an unrelated process by the time this runs, and that
        /// unrelated process is what gets put in the job and killed with it.
        ///
        /// So: pass only the pid of a child this caller spawned, keep the
        /// `Child` (the handle) alive across this call, and do not `wait` on it
        /// first. The caller keeps owning the child afterwards — the guard owns
        /// the job object, never the process — and must go on waiting on it as
        /// usual.
        ///
        /// # Lifetime
        ///
        /// The assignment lasts as long as the process does: a process cannot
        /// leave a job. Dropping the guard closes the job and kills everything
        /// in it, so hold the guard for at least as long as this process is
        /// wanted alive.
        ///
        /// On failure the job holds whatever it held before, so the caller
        /// still owns the process and can kill it directly; a guard that never
        /// took a process terminates nothing when dropped.
        ///
        /// There is a small race: descendants the process spawns between its
        /// creation and this assignment escape the job. Closing it fully would
        /// require creating the process suspended (`CREATE_SUSPENDED`),
        /// assigning it, then resuming it, which the std/async process APIs
        /// don't support without reimplementing process creation. The window is
        /// microseconds, and the children we care about (`npx`, `node`, `ssh`,
        /// etc.) take far longer to load their runtime and spawn anything, so
        /// in practice nothing escapes.
        pub fn assign_process(&self, pid: u32) -> Result<()> {
            unsafe {
                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid)
                    .with_context(|| format!("failed to open process {pid}"))?;
                let result = AssignProcessToJobObject(self.0, process)
                    .with_context(|| format!("failed to assign process {pid} to job object"));
                CloseHandle(process).log_err();
                result
            }
        }

        /// Kills the whole tree without waiting for the guard to be dropped.
        pub fn terminate(&self) -> Result<()> {
            unsafe { TerminateJobObject(self.0, 1).context("failed to terminate job object") }
        }
    }

    impl Drop for ProcessTreeGuard {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0).log_err();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever it picks has to be spawnable *into*: an absolute path to a
    /// directory that exists right now. The fallback answers to the same
    /// contract, so it is checked here too rather than only on the machines
    /// that happen to have no home directory.
    #[test]
    fn the_stable_child_dir_and_its_fallback_are_existing_absolute_directories() {
        for directory in [stable_child_dir(), std::env::temp_dir()] {
            assert!(directory.is_absolute(), "{directory:?} is not absolute");
            assert!(directory.is_dir(), "{directory:?} is not a directory");
        }
    }

    #[test]
    fn the_stable_child_dir_is_usable_as_a_child_working_directory() {
        let directory = stable_child_dir();
        let mut command =
            smol::process::Command::new(if cfg!(windows) { "cmd.exe" } else { "pwd" });
        if cfg!(windows) {
            command.args(["/D", "/C", "cd"]);
        }
        let output = smol::block_on(command.current_dir(&directory).output())
            .expect("failed to spawn a child in the stable directory");

        assert!(output.status.success(), "child failed: {output:?}");
        let reported = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
        assert_eq!(
            std::fs::canonicalize(reported).unwrap(),
            std::fs::canonicalize(directory).unwrap()
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Builds a command running `powershell -> ping`, where the powershell
    /// process writes the pid of its `ping` grandchild to `pid_file`.
    ///
    /// When `start_file` is supplied, the root waits for that file before it
    /// creates the grandchild. This lets a test assign the root to a job
    /// object immediately after spawn, closing the otherwise unavoidable
    /// post-spawn race before descendants are created.
    fn process_tree_command(
        pid_file: &std::path::Path,
        start_file: Option<&std::path::Path>,
    ) -> std::process::Command {
        let mut command = std::process::Command::new("powershell.exe");
        let wait_for_start = start_file.map(|path| {
            format!(
                "$deadline = (Get-Date).AddSeconds(5); while (!(Test-Path -LiteralPath '{}')) {{ if ((Get-Date) -gt $deadline) {{ exit 1 }}; Start-Sleep -Milliseconds 10 }}; ",
                path.display()
            )
        });
        command.args(["-NoProfile", "-Command"]).arg(format!(
            "{}$p = Start-Process -FilePath ping.exe -ArgumentList @('-n','60','127.0.0.1') -PassThru -WindowStyle Hidden; \
             Set-Content -LiteralPath '{}' -Value $p.Id; \
             Wait-Process -Id $p.Id",
            wait_for_start.unwrap_or_default(),
            pid_file.display()
        ));
        command
    }

    /// Spawns a process tree `powershell -> ping` via `Child::spawn` and
    /// returns the `Child` along with the pid of the grandchild (`ping`).
    fn spawn_process_tree(temp_dir: &std::path::Path) -> (Child, u32) {
        let pid_file = temp_dir.join("grandchild_pid");
        let child = Child::spawn(
            process_tree_command(&pid_file, None),
            Stdio::null(),
            Stdio::null(),
            Stdio::null(),
        )
        .expect("failed to spawn powershell");
        let grandchild_pid = wait_for_grandchild_pid(&pid_file);
        (child, grandchild_pid)
    }

    fn spawn_unguarded_process_tree(
        temp_dir: &std::path::Path,
        name: &str,
        guard: &ProcessTreeGuard,
    ) -> (smol::process::Child, u32) {
        let pid_file = temp_dir.join(name);
        let start_file = temp_dir.join(format!("{name}.start"));
        let mut command =
            smol::process::Command::from(process_tree_command(&pid_file, Some(&start_file)));
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn powershell");
        guard
            .assign_process(child.id())
            .expect("failed to assign powershell to guard");
        std::fs::write(&start_file, b"start").expect("failed to release process tree");
        let grandchild_pid = wait_for_grandchild_pid(&pid_file);
        (child, grandchild_pid)
    }

    fn wait_for_grandchild_pid(pid_file: &std::path::Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        let grandchild_pid = loop {
            if let Ok(contents) = std::fs::read_to_string(pid_file)
                && let Ok(pid) = contents.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for grandchild pid file"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(
            process_is_alive(grandchild_pid),
            "grandchild should be alive after spawning"
        );
        grandchild_pid
    }

    fn process_is_alive(pid: u32) -> bool {
        use windows::Win32::{
            Foundation::{CloseHandle, STILL_ACTIVE},
            System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        };

        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return false;
            };
            let mut exit_code = 0u32;
            let alive = GetExitCodeProcess(handle, &mut exit_code).is_ok()
                && exit_code == STILL_ACTIVE.0 as u32;
            CloseHandle(handle).expect("failed to close process handle");
            alive
        }
    }

    fn assert_process_exits(pid: u32, message: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_is_alive(pid) {
            assert!(Instant::now() < deadline, "{message} (pid {pid})");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn test_kill_terminates_grandchildren() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (mut child, grandchild_pid) = spawn_process_tree(temp_dir.path());

        child.kill().expect("failed to kill child");

        assert_process_exits(
            grandchild_pid,
            "grandchild should be terminated after killing the child",
        );
    }

    #[test]
    fn test_drop_terminates_grandchildren() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (child, grandchild_pid) = spawn_process_tree(temp_dir.path());

        drop(child);

        assert_process_exits(
            grandchild_pid,
            "grandchild should be terminated after dropping the child",
        );
    }

    #[test]
    fn test_dropping_guard_terminates_a_separately_spawned_tree() {
        let temp_dir = tempfile::tempdir().unwrap();
        let guard = ProcessTreeGuard::new().expect("failed to create guard");
        let (mut child, grandchild_pid) =
            spawn_unguarded_process_tree(temp_dir.path(), "grandchild_pid", &guard);

        drop(guard);

        assert_process_exits(
            child.id(),
            "child should be terminated after dropping the guard",
        );
        assert_process_exits(
            grandchild_pid,
            "grandchild should be terminated after dropping the guard",
        );
        smol::block_on(child.status()).expect("failed to reap child");
    }

    #[test]
    fn test_terminating_guard_terminates_the_entire_tree() {
        let temp_dir = tempfile::tempdir().unwrap();
        let guard = ProcessTreeGuard::new().expect("failed to create guard");
        let (mut child, grandchild_pid) =
            spawn_unguarded_process_tree(temp_dir.path(), "terminate_grandchild_pid", &guard);
        let child_pid = child.id();

        guard.terminate().expect("failed to terminate job");

        assert_process_exits(child_pid, "direct child survived explicit termination");
        assert_process_exits(grandchild_pid, "grandchild survived explicit termination");
        smol::block_on(child.status()).expect("failed to reap child");
        drop(guard);
    }

    #[test]
    fn test_one_guard_terminates_every_assigned_tree() {
        let temp_dir = tempfile::tempdir().unwrap();
        let guard = ProcessTreeGuard::new().expect("failed to create guard");
        let (mut first, first_grandchild) =
            spawn_unguarded_process_tree(temp_dir.path(), "first_grandchild_pid", &guard);
        let (mut second, second_grandchild) =
            spawn_unguarded_process_tree(temp_dir.path(), "second_grandchild_pid", &guard);
        let first_pid = first.id();
        let second_pid = second.id();

        drop(guard);

        for (pid, description) in [
            (first_pid, "first direct child"),
            (first_grandchild, "first grandchild"),
            (second_pid, "second direct child"),
            (second_grandchild, "second grandchild"),
        ] {
            assert_process_exits(pid, &format!("{description} survived guard drop"));
        }
        smol::block_on(first.status()).expect("failed to reap first child");
        smol::block_on(second.status()).expect("failed to reap second child");
    }

    #[test]
    fn test_assigning_an_invalid_pid_fails_without_poisoning_the_guard() {
        let guard = ProcessTreeGuard::new().expect("failed to create guard");
        let error = guard
            .assign_process(u32::MAX)
            .expect_err("an impossible pid should not be assignable");
        assert!(
            format!("{error:#}").contains("failed to open process"),
            "unexpected assignment error: {error:#}"
        );

        let temp_dir = tempfile::tempdir().unwrap();
        let (mut child, grandchild_pid) =
            spawn_unguarded_process_tree(temp_dir.path(), "after_failure_grandchild_pid", &guard);
        let child_pid = child.id();
        drop(guard);
        assert_process_exits(child_pid, "child survived guard drop");
        assert_process_exits(grandchild_pid, "grandchild survived guard drop");
        smol::block_on(child.status()).expect("failed to reap child");
    }

    #[test]
    fn test_output_keeps_the_guard_alive_until_output_is_collected() {
        let mut command = std::process::Command::new("cmd.exe");
        command.args(["/D", "/C", "echo guarded-output"]);
        let child = Child::spawn(command, Stdio::null(), Stdio::piped(), Stdio::piped())
            .expect("failed to spawn output child");

        let output = smol::block_on(child.output()).expect("failed to collect output");

        assert!(output.status.success(), "child failed: {output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            "guarded-output"
        );
    }
}
