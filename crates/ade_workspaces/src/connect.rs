//! A fresh window reattaches to (or creates) its daemon workspace.
//!
//! The first window on a project used to open a plain centre-pane shell that
//! died with the window, so opening it twice left two throwaway shells and
//! nothing to come back to. This module is the other branch of that
//! fresh-window decision: a window ensures the session daemon its project
//! belongs to — this machine's for a local project, the host's for an ssh one —
//! adopts whatever that daemon already holds, and then either reattaches to the
//! project's most recently opened workspace or creates the first, exactly as
//! the workspace panel's "Add workspace" would. Every centre-panel terminal in
//! the window is a daemon session from then on, outliving the window.
//!
//! Opening attaches: it builds the daemon's layout and attaches to the
//! sessions it names, so a second open reattaches rather than spawning, and a
//! session that died with its host still surfaces as gone.
//!
//! **WSL and Docker keep the plain terminal.** ADE's session layer takes an
//! ssh destination or nothing at all, and those transports have neither. (A
//! WSL box reached *over ssh* is an ssh host, and takes the workspace path.)
//! A daemon that cannot be reached has answered nothing and keeps the plain
//! terminal too, with a log line: opening a project must never cost the user
//! their shell.

use crate::{
    WorkspaceLifecycleService, daemon_backend::Outdated, open_workspace_session,
    store::AdeWorkspaceStore,
};
use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, EntityId, Global, PromptLevel, WeakEntity,
    Window,
};
use remote::{RemoteConnectionOptions, SshConnectionOptions};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use workspace::Workspace;

/// Windows the connect flow has already claimed, by workspace entity id.
///
/// Two callers can legitimately reach for the same fresh connection — the
/// fresh-window item when nothing was serialized, and the sidebar's
/// workspace-added hook when something was — and the flow must run once per
/// window, not once per caller. The first claim wins; the second caller is
/// told `true` so it opens nothing over the flow already in flight.
#[derive(Default)]
struct ClaimedWindows(HashSet<EntityId>);

impl Global for ClaimedWindows {}

pub(crate) fn release_window_claim(window: EntityId, cx: &mut App) {
    if cx.has_global::<ClaimedWindows>() {
        cx.global_mut::<ClaimedWindows>().0.remove(&window);
    }
}

/// OpenSSH's own default, and what `~/.ssh/config` resolution assumes when the
/// settings leave the port unset.
const DEFAULT_SSH_PORT: u16 = 22;

/// What the user is told when this machine's daemon speaks a protocol the
/// client does not; there is no upgrade offer, so the message has to stand on
/// its own.
const INCOMPATIBLE_LOCAL_DAEMON: &str = "The session daemon on this machine uses an incompatible \
     protocol. This window has no persistent session until that daemon is replaced.";

/// The other direction: the daemon is *newer* than this client.
///
/// No daemon action is offered, and none may be: deploying this client's binary
/// over a newer daemon is a downgrade that would kill every session it holds to
/// install older bytes.
const CLIENT_TOO_OLD: &str = "The session daemon uses a newer protocol than this client. Update \
     this client to use its sessions; replacing the daemon would terminate everything running \
     through it.";

/// The daemon a window's sessions belong to.
///
/// Everything the flow does differently for the two — which daemon to ensure,
/// which registry rows can match, what a created workspace records, and which
/// root is the user's whole account rather than a project — is decided here, so
/// the flow itself reads the same either way.
#[derive(Clone)]
enum WindowDaemonHost {
    Local,
    Ssh(SshConnectionOptions),
}

impl WindowDaemonHost {
    /// The daemon host for a window's project, or `None` when ADE has no
    /// session layer to offer it. The local daemon is Unix-only at this stage;
    /// tests keep the local path available for their fake backend.
    fn for_connection(options: Option<RemoteConnectionOptions>) -> Option<Self> {
        match options {
            None if cfg!(any(unix, test)) => Some(Self::Local),
            Some(RemoteConnectionOptions::Ssh(ssh)) => Some(Self::Ssh(ssh)),
            _ => None,
        }
    }

    /// What a workspace created here records as its `remote_host`, and the key
    /// the lifecycle layer names a backend by. `None` **is** this machine.
    fn destination(&self) -> Option<String> {
        match self {
            Self::Local => None,
            Self::Ssh(ssh) => Some(ssh_destination(ssh)),
        }
    }

    /// How this host reads in a log line.
    fn label(&self) -> String {
        self.destination()
            .unwrap_or_else(|| "this machine".to_owned())
    }

    /// Whether the project root **is** the user's home directory rather than a
    /// project inside it, which is not a workspace (operator ruling,
    /// 2026-08-05).
    fn is_home_directory(&self, root: &Path) -> bool {
        match self {
            Self::Local => same_local_path(root, util::paths::home_dir()),
            Self::Ssh(ssh) => is_remote_home_directory(root, ssh),
        }
    }

    /// Whether a workspace is one this window could reattach to: the same
    /// daemon, the same project root.
    ///
    /// Over the two fields rather than over [`AdeWorkspace`], because the
    /// candidates are both registry rows and records a host merely holds, and
    /// a discovered record has no row to pass.
    fn holds(&self, remote_host: Option<&str>, repository_path: &Path, root: &Path) -> bool {
        match self {
            Self::Local => remote_host.is_none() && same_local_path(repository_path, root),
            Self::Ssh(ssh) => host_matches_destination(ssh, remote_host) && repository_path == root,
        }
    }
}

/// Daemons this session has already been told to leave alone: an incompatible
/// one on this machine, or a remote upgrade the user answered with Cancel.
///
/// Every trigger re-runs the flow — a workspace switch, a terminal click — so
/// without this the same modal or toast comes back on each one. Sticky until
/// the app restarts, like the lifecycle service's `status_errors`: a refusal is
/// a standing decision, not a failed action for the next attempt to clear. An
/// ensure that merely failed, or an upgrade that was attempted and broke, is
/// **not** a refusal — those are worth retrying.
#[derive(Default)]
struct RefusedDaemons(HashSet<Option<String>>);

impl Global for RefusedDaemons {}

/// Records that this host's daemon is not to be asked again this session: its
/// windows get stock behavior for the rest of the run — which is also the seam
/// a test suite predating local adoption uses to turn the flow off.
pub fn refuse_daemon(host: Option<String>, cx: &mut App) {
    cx.default_global::<RefusedDaemons>().0.insert(host);
}

/// Takes over a fresh window: `true` means this window now belongs to the
/// connect flow (which opens a workspace, or falls back to a plain terminal
/// itself), `false` means the window is not ADE's to take — WSL or Docker — and
/// the caller should open whatever a fresh window normally gets.
pub fn open_connection_workspace(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let project = workspace.project().clone();
    let Some(host) =
        WindowDaemonHost::for_connection(project.read(cx).remote_connection_options(cx))
    else {
        return false;
    };
    let destination = host.destination();
    let label = host.label();
    // Before the claim: a refused daemon means this window gets stock behavior,
    // with no ensure and no second prompt.
    if cx
        .try_global::<RefusedDaemons>()
        .is_some_and(|refused| refused.0.contains(&destination))
    {
        return false;
    }
    let window_id = cx.entity().entity_id();
    if !cx.default_global::<ClaimedWindows>().0.insert(window_id) {
        return true;
    }
    // The one line that proves the flow started; everything after it either
    // attaches or explains itself in this same log.
    log::info!("ADE claims the {label} window; reattaching once the project root settles");

    // A window restored at startup fires its workspace-added hook before the
    // project's worktrees have loaded, so the project root cannot be read here
    // — it is awaited inside the flow. How patient to be is decided by what
    // the window already shows: a restored layout gives the user something to
    // look at while a slow host connects; an empty window owes them a shell
    // fast, and a window opened with no folder at all never grows a worktree,
    // which the short deadline turns into the plain terminal.
    let restored = workspace
        .panes()
        .iter()
        .any(|pane| pane.read(cx).items_len() > 0);
    let root_deadline = if restored {
        std::time::Duration::from_secs(120)
    } else {
        std::time::Duration::from_secs(3)
    };

    let lifecycle = crate::lifecycle_service(cx);
    cx.spawn_in(window, async move |this, cx| {
        let Some(repository_path) = wait_for_project_root(&this, root_deadline, cx).await else {
            log::info!(
                "ADE waited {root_deadline:?} for {label}'s project root; \
                 this window gets a plain terminal if it is still empty"
            );
            fall_back_to_plain_terminal(&this, window_id, cx);
            return;
        };
        // Opening with nothing but `~` filled in is how the remote picker
        // behaves before a folder is chosen, and how a local window opened on
        // the home folder looks. Off the UI thread: the local answer
        // canonicalizes, and a sleeping UNC or mapped-drive root would freeze
        // the window for the SMB timeout.
        let at_home = cx
            .background_spawn({
                let host = host.clone();
                let repository_path = repository_path.clone();
                async move { host.is_home_directory(&repository_path) }
            })
            .await;
        if at_home {
            log::info!("the {label} window is rooted at the home directory, which is not a project");
            fall_back_to_plain_terminal(&this, window_id, cx);
            return;
        }

        // Blocking, deliberately: the ensure drives the host's connection and
        // adoption reads the daemon, then sqlite.
        let listed = cx
            .background_spawn({
                let lifecycle = lifecycle.clone();
                let destination = destination.clone();
                async move {
                    lifecycle
                        .ensure_host_workspaces(destination.as_deref())
                        .await
                }
            })
            .await;

        let incompatible = listed
            .as_ref()
            .err()
            .and_then(crate::daemon_backend::incompatible_daemon);
        let listed = match incompatible {
            // Not reachability: the two ends cannot speak to each other, and
            // every exit below leaves the window unresolved rather than opening
            // a plain terminal (§6.1 — no competing shell while a daemon may
            // still own an agent's writer).
            Some(outdated) => {
                log::warn!(
                    "ADE found an incompatible session daemon on {label} ({outdated:?} is behind): {:#}",
                    listed.as_ref().err().expect("an incompatibility is a failure"),
                );
                let upgraded = route_incompatible_daemon(
                    Some(&this),
                    outdated,
                    destination.clone(),
                    &lifecycle,
                    cx,
                )
                .await;
                if !upgraded {
                    leave_ade_unresolved(window_id, cx);
                    return;
                }
                let destination = destination.clone();
                cx.background_spawn({
                    let lifecycle = lifecycle.clone();
                    async move {
                        lifecycle
                            .ensure_host_workspaces(destination.as_deref())
                            .await
                    }
                })
                .await
            }
            None => listed,
        };

        if let Err(error) = listed {
            // The re-ensure after an upgrade that reported success. Still
            // incompatible means the upgrade did not fix it, which is a failed
            // upgrade and takes the same no-terminal exit.
            if crate::daemon_backend::incompatible_daemon(&error).is_some() {
                log::warn!("{label}'s daemon is still incompatible after the upgrade: {error:#}");
                leave_ade_unresolved(window_id, cx);
                return;
            }
            log::warn!(
                "ADE could not reach {label}, so this window gets a plain terminal: {error:#}"
            );
            fall_back_to_plain_terminal(&this, window_id, cx);
            return;
        }

        // The daemon has just been contacted, so whatever it holds can become
        // panel rows now rather than at the next thing that asks.
        cx.update(|_, cx| {
            if let Some(store) = AdeWorkspaceStore::try_global(cx) {
                store.update(cx, |store, cx| store.refresh(cx));
            }
        })
        .ok();

        // The lifecycle service owns the decision, so its registry read and its
        // create cannot be interleaved by another window on this root.
        let host_destination = destination.clone();
        let adopted = cx
            .background_spawn({
                let lifecycle = lifecycle.clone();
                let root = repository_path.clone();
                async move {
                    lifecycle
                        .adopt_or_create_workspace(
                            repository_path,
                            destination,
                            move |remote_host, repository_path| {
                                host.holds(remote_host, repository_path, &root)
                            },
                        )
                        .await
                }
            })
            .await;

        let opened = match adopted {
            Ok((created, true)) => open_in_window(&this, created.id, cx).await,
            Ok((existing, false)) => open_in_window(&this, existing.id, cx).await,
            Err(error) => Err(error),
        };

        match opened {
            Err(error) => {
                // An incompatibility can surface *here* rather than at the
                // listing — the host's daemon replaced in the gap, by another
                // client's upgrade — and §6.1 does not care which phase found
                // it: same surface, and no competing shell beside a daemon that
                // may still own an agent's writer.
                if let Some(outdated) = crate::daemon_backend::incompatible_daemon(&error) {
                    log::warn!(
                        "opening the ADE workspace for {label} met an incompatible daemon \
                         ({outdated:?} is behind): {error:#}"
                    );
                    route_incompatible_daemon(
                        Some(&this),
                        outdated,
                        host_destination,
                        &lifecycle,
                        cx,
                    )
                    .await;
                    leave_ade_unresolved(window_id, cx);
                    return;
                }
                // The window must not come up empty: whatever the workspace path
                // could not deliver, the window still owes the user a shell.
                log::warn!("opening the ADE workspace for {label} failed: {error:#}");
                fall_back_to_plain_terminal(&this, window_id, cx);
            }
            // `Ok` is not proof that ADE took the window: opening a workspace
            // whose session re-probes dead installs no layout and no sync and
            // still succeeds. Keeping the claim on a window ADE does not own
            // would freeze it — every later trigger sees the claim and does
            // nothing.
            Ok(()) => {
                let owned = this
                    .read_with(cx, |workspace, _| workspace.ade_owns_layout())
                    .unwrap_or(false);
                if !owned {
                    log::info!(
                        "the ADE workspace for {label} opened without taking the window; \
                         falling back to a plain terminal"
                    );
                    fall_back_to_plain_terminal(&this, window_id, cx);
                }
            }
        }
    })
    .detach();
    true
}

/// Tell the user about a daemon this client cannot speak to, and offer the one
/// repair that is not a downgrade.
///
/// `true` means an upgrade was accepted **and** reported success, so the caller
/// may retry what the incompatibility cost it. Every other outcome — a daemon
/// nothing may replace, a cancel, a failed upgrade — has already been surfaced,
/// and the caller owes the window nothing but the unresolved exit.
async fn route_incompatible_daemon(
    this: Option<&WeakEntity<Workspace>>,
    outdated: Outdated,
    destination: Option<String>,
    lifecycle: &std::sync::Arc<WorkspaceLifecycleService>,
    cx: &mut AsyncWindowContext,
) -> bool {
    let Some(upgradable) = upgradable_host(outdated, destination.clone()) else {
        // Either this machine's daemon — which ships with the client, so an
        // incompatible one is a broken install with no remote deploy to answer
        // for — or a daemon newer than us, which nothing here may replace.
        let message = match outdated {
            Outdated::Daemon => INCOMPATIBLE_LOCAL_DAEMON,
            Outdated::Client => CLIENT_TOO_OLD,
        };
        cx.update(|_, cx| refuse_daemon(destination, cx)).ok();
        // A window of its own for the flow that has one; a modal for the
        // mid-session watcher, which is not attached to any workspace.
        match this {
            Some(this) => show_error(this, message, cx),
            None => tell("The session daemon is incompatible", message, cx).await,
        }
        return false;
    };
    match offer_incompatible_daemon_upgrade(&upgradable, lifecycle.clone(), cx).await {
        Ok(true) => true,
        Ok(false) => {
            cx.update(|_, cx| refuse_daemon(Some(upgradable), cx)).ok();
            false
        }
        Err(error) => {
            log::warn!("upgrading the session daemon on {upgradable} failed: {error:#}");
            tell(
                "Failed to upgrade the remote session daemon",
                &format!("{error:#}"),
                cx,
            )
            .await;
            false
        }
    }
}

/// The one consumer of mid-session stream incompatibility, for the whole app.
///
/// **App-global on purpose.** Every open window watches the same hosts, so a
/// per-window watcher would stack one modal per window on the one daemon that
/// went incompatible. The dedupe itself lives in the lifecycle service
/// ([`WorkspaceLifecycleService::take_stream_incompatibility`]), which hands
/// each down epoch out once.
pub(crate) fn watch_daemon_incompatibility(cx: &mut App) {
    if cx.has_global::<IncompatibilityWatcher>() {
        return;
    }
    let lifecycle = crate::lifecycle_service(cx);
    let changes = lifecycle.watch_daemon_freshness();
    let watcher = cx.spawn(async move |cx| {
        while changes.recv().await.is_ok() {
            while let Some((host, outdated)) = lifecycle.take_stream_incompatibility() {
                let label = host.clone().unwrap_or_else(|| "this machine".to_owned());
                log::warn!(
                    "the session daemon on {label} became incompatible mid-session \
                     ({outdated:?} is behind)"
                );
                let lifecycle = lifecycle.clone();
                let shown = cx.update(|cx| {
                    let Some(handle) = cx.active_window() else {
                        return false;
                    };
                    handle
                        .update(cx, |_, window, cx| {
                            window
                                .spawn(cx, async move |cx| {
                                    route_incompatible_daemon(None, outdated, host, &lifecycle, cx)
                                        .await
                                })
                                .detach();
                        })
                        .is_ok()
                });
                if !shown {
                    // No window to put it in; the host error the sidebar draws
                    // is what is left, and the next down epoch tries again.
                    log::warn!("no window was available to report {label}'s incompatibility");
                }
            }
        }
    });
    cx.set_global(IncompatibilityWatcher(watcher));
}

/// Holds [`watch_daemon_incompatibility`]'s task for the life of the app.
struct IncompatibilityWatcher(#[allow(dead_code)] gpui::Task<()>);

impl Global for IncompatibilityWatcher {}

/// The host an incompatibility may offer to replace the daemon on.
///
/// **A daemon newer than this client is never one of them**, whatever else is
/// true: deploying our binary over it is a downgrade that terminates every
/// session it holds to install older bytes. `None` also for this machine, whose
/// daemon ships with the app and has no remote deploy to answer for.
fn upgradable_host(outdated: Outdated, destination: Option<String>) -> Option<String> {
    destination.filter(|_| outdated == Outdated::Daemon)
}

/// Ask before replacing a daemon when doing so will terminate its sessions.
async fn offer_incompatible_daemon_upgrade(
    destination: &str,
    lifecycle: std::sync::Arc<WorkspaceLifecycleService>,
    cx: &mut AsyncWindowContext,
) -> anyhow::Result<bool> {
    let detail = format!(
        "The session daemon on {destination} uses an incompatible protocol. Upgrading it will \
         terminate every terminal and agent currently running through that daemon."
    );
    let answer = cx
        .update(|window, cx| {
            window.prompt(
                PromptLevel::Critical,
                "Remote session daemon is incompatible",
                Some(&detail),
                &["Terminate sessions and upgrade", "Cancel"],
                cx,
            )
        })?
        .await?;
    if answer != 0 {
        return Ok(false);
    }

    let destination = destination.to_owned();
    let outcome = cx
        .background_spawn(async move { lifecycle.upgrade_host_daemon(&destination) })
        .await?;
    log::info!("session daemon upgrade completed: {outcome:?}");
    Ok(true)
}

/// The first visible worktree's root, awaited because a restored window loads
/// its worktrees after the workspace exists. `None` means the deadline passed
/// with the project still rootless.
async fn wait_for_project_root(
    this: &WeakEntity<Workspace>,
    deadline: std::time::Duration,
    cx: &mut AsyncWindowContext,
) -> Option<std::path::PathBuf> {
    let poll = std::time::Duration::from_millis(250);
    let mut waited = std::time::Duration::ZERO;
    loop {
        let root = this
            .update(cx, |workspace, cx| {
                let project = workspace.project().read(cx);
                project
                    .visible_worktrees(cx)
                    .next()
                    .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            })
            .ok()?;
        if let Some(root) = root {
            return Some(root);
        }
        if waited >= deadline {
            return None;
        }
        cx.background_executor().timer(poll).await;
        waited += poll;
    }
}

/// Opens workspace `id` in this window through the one entry point every
/// surface uses, so a connect means the same thing as clicking the row.
async fn open_in_window(
    this: &WeakEntity<Workspace>,
    id: crate::WorkspaceId,
    cx: &mut AsyncWindowContext,
) -> anyhow::Result<()> {
    this.update_in(cx, |_, window, cx| {
        open_workspace_session(&cx.entity(), id, window, cx)
    })?
    .await
}

/// Ends the flow without an ADE session.
///
/// The claim goes back first, so a window that fell back can be run through the
/// flow again — a reconnect, a re-add, the user reopening the project — rather
/// than being stuck outside ADE for the rest of the session.
///
/// Then the window gets what ADE could not give it: the same centre-pane
/// terminal a fresh, empty window opens with, and one that dies with the
/// window. Only into an **empty** window — a restored layout already gives the
/// user their shells, and stacking one more on top of it would be noise.
fn fall_back_to_plain_terminal(
    this: &WeakEntity<Workspace>,
    window_id: EntityId,
    cx: &mut AsyncWindowContext,
) {
    #[cfg(test)]
    PLAIN_TERMINALS.with(|count| count.set(count.get() + 1));
    cx.update(|_, cx| release_window_claim(window_id, cx)).ok();
    this.update_in(cx, |workspace, window, cx| {
        let occupied = workspace
            .panes()
            .iter()
            .any(|pane| pane.read(cx).items_len() > 0);
        if occupied {
            return;
        }
        // A fake fs has no real PTY behind it; its reader thread would also
        // violate the deterministic test scheduler.
        if workspace.project().read(cx).fs().is_fake() {
            return;
        }
        terminal_view::TerminalView::deploy(
            workspace,
            &workspace::NewCenterTerminal::default(),
            window,
            cx,
        );
    })
    .ok();
}

// Plain terminals the flows on this thread have deployed. A test window has no
// pty, so the terminal itself never materialises and counting panes cannot tell
// a fallback from an abstention — which is exactly the distinction §6.1 turns
// on. Thread-local because a `TestAppContext`'s executor is driven by its own
// test thread.
#[cfg(test)]
thread_local! {
    static PLAIN_TERMINALS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Ends the flow with the window's ADE state unresolved: no session, and
/// deliberately **no plain terminal either**.
///
/// `docs/ade/protocol-compatibility.md` §6.1: a daemon this client cannot talk
/// to may still own the writer of a live agent, and a second shell on the same
/// work is worse than none. The user has just been shown what is wrong; the
/// claim goes back so a later trigger can retry once they have acted on it.
fn leave_ade_unresolved(window_id: EntityId, cx: &mut AsyncWindowContext) {
    cx.update(|_, cx| release_window_claim(window_id, cx)).ok();
}

/// One modal with nothing to decide, awaited. A window that cannot even show it
/// is logged and moved past: the flow has already decided what it is doing.
async fn tell(title: &str, detail: &str, cx: &mut AsyncWindowContext) {
    let prompt = cx.update(|window, cx| {
        window.prompt(PromptLevel::Critical, title, Some(detail), &["OK"], cx)
    });
    let shown = match prompt {
        Ok(prompt) => prompt.await.map(|_| ()).map_err(anyhow::Error::from),
        Err(error) => Err(error),
    };
    if let Err(error) = shown {
        log::warn!("showing {title:?} failed: {error:#}");
    }
}

/// Shows the window one error, in the place every other ADE failure surfaces.
fn show_error(this: &WeakEntity<Workspace>, message: &'static str, cx: &mut AsyncWindowContext) {
    log::error!("{message}");
    this.update(cx, |workspace, cx| workspace.show_error(message, cx))
        .ok();
}

/// Whether a workspace's stored destination names the host this window is
/// connected to. Compared by the parts ssh(1) resolves — host, username, port —
/// with the unstated ones lenient, because a destination may be spelled several
/// ways and the stored string is whatever the workspace was created with.
fn host_matches_destination(ssh: &SshConnectionOptions, destination: Option<&str>) -> bool {
    let Some(destination) = destination else {
        return false;
    };
    let destination = parse_ssh_destination(destination);
    destination.host.eq_ignore_ascii_case(&ssh.host.to_string())
        && destination
            .username
            .is_none_or(|username| ssh.username.as_deref() == Some(username))
        && destination
            .port
            .is_none_or(|port| ssh.port.unwrap_or(DEFAULT_SSH_PORT) == port)
}

/// The ADE host string for a Zed remote connection — the same one a workspace
/// created on it records as its `remote_host`, and the key the lifecycle layer
/// names a backend by.
///
/// `None` for anything that is not ssh: those connections have no host running
/// a session daemon, so there is nothing to name.
pub fn destination_for(options: &RemoteConnectionOptions) -> Option<String> {
    match options {
        RemoteConnectionOptions::Ssh(ssh) => Some(ssh_destination(ssh)),
        _ => None,
    }
}

/// The destination string `ssh` accepts for these options — what a created
/// workspace records as its `remote_host`.
fn ssh_destination(options: &SshConnectionOptions) -> String {
    let username = options.username.as_deref();
    match options.port.filter(|port| *port != DEFAULT_SSH_PORT) {
        Some(port) => {
            let host = options.host.to_bracketed_string();
            match username {
                Some(username) => format!("ssh://{username}@{host}:{port}"),
                None => format!("ssh://{host}:{port}"),
            }
        }
        None => {
            let host = options.host.to_string();
            match username {
                Some(username) => format!("{username}@{host}"),
                None => host,
            }
        }
    }
}

/// A destination broken back into the parts [`ssh_destination`] wrote it from.
#[derive(Debug, PartialEq, Eq)]
struct SshDestination<'a> {
    username: Option<&'a str>,
    host: &'a str,
    port: Option<u16>,
}

/// Reads a destination — a workspace's stored `remote_host` — back into its
/// parts. Deliberately lenient in the same places ssh(1) is: everything but the
/// host is optional, and anything that does not parse as a port stays part of
/// the host rather than being dropped.
fn parse_ssh_destination(destination: &str) -> SshDestination<'_> {
    let destination = destination
        .strip_prefix("ssh://")
        .unwrap_or(destination)
        .trim();
    // The last `@`, because a username may not contain one but a host never
    // does.
    let (username, rest) = match destination.rsplit_once('@') {
        Some((username, rest)) => (Some(username), rest),
        None => (None, destination),
    };

    // A bracketed IPv6 literal is the only form where the host itself holds
    // colons, so it is the only one where a trailing `:port` can be split off
    // unambiguously.
    let (host, port) = match rest.strip_prefix('[').and_then(|rest| rest.split_once(']')) {
        Some((host, after)) => (host, after.strip_prefix(':')),
        None => match rest.split_once(':') {
            // Two colons or more without brackets is a bare IPv6 literal, not
            // a port.
            Some((host, port)) if !port.contains(':') => (host, Some(port)),
            _ => (rest, None),
        },
    };

    match port.map(str::parse::<u16>) {
        Some(Ok(port)) => SshDestination {
            username,
            host,
            port: Some(port),
        },
        // Not a port after all: put the string back rather than losing it.
        Some(Err(_)) => SshDestination {
            username,
            host: rest,
            port: None,
        },
        None => SshDestination {
            username,
            host,
            port: None,
        },
    }
}

/// Whether a connection's project root **is** the remote user's home
/// directory, rather than a project inside it. Exact equality only: a path
/// *under* home is an ordinary checkout. The username comes from the
/// connection's own options; one that states no username is left alone, since
/// ssh resolves it from the config and guessing would refuse a real project.
fn is_remote_home_directory(project_root: &Path, ssh: &SshConnectionOptions) -> bool {
    let Some(user) = ssh.username.as_deref().filter(|user| !user.is_empty()) else {
        return false;
    };
    // The two layouts a POSIX host puts accounts under, plus root's, which is
    // beside them rather than in them.
    let mut homes = vec![format!("/home/{user}"), format!("/Users/{user}")];
    if user == "root" {
        homes.push("/root".to_owned());
    }
    homes
        .iter()
        .any(|home| project_root == Path::new(home.as_str()))
}

/// Whether two paths **on this machine** name the same directory.
///
/// Compared canonical, because a project root arrives however the OS spelled it
/// — another drive-letter case, a symlink, a `\\?\` prefix — while the registry
/// holds whatever the workspace was created with, and a spelling difference
/// would mint a second workspace for a project that already has one. A path
/// that no longer resolves is compared as written. **Local paths only**: a
/// remote path resolved against this filesystem would be nonsense.
fn same_local_path(left: &Path, right: &Path) -> bool {
    left == right || canonical_local(left) == canonical_local(right)
}

fn canonical_local(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .map(|path| PathBuf::from(util::paths::SanitizedPath::new(&path)))
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AdeWorkspace, AdeWorkspaceRegistry, Attached, BackendWorkspace, DaemonEvent,
        DaemonUpgradeOutcome, GlobalLifecycleService, SessionBackend, SessionId, SessionInfo,
        SessionSpec, StatusDelivery,
    };
    use anyhow::bail;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use remote::{DockerConnectionOptions, WslConnectionOptions};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Default)]
    struct UpgradeBackend {
        upgrades: AtomicUsize,
    }

    impl SessionBackend for UpgradeBackend {
        fn create(&self, _spec: &SessionSpec) -> anyhow::Result<SessionId> {
            bail!("not used")
        }

        fn list(&self) -> anyhow::Result<Vec<SessionInfo>> {
            Ok(Vec::new())
        }

        fn exists(&self, _id: &SessionId) -> anyhow::Result<bool> {
            Ok(false)
        }

        fn attach(&self, _spec: &SessionSpec) -> anyhow::Result<Attached> {
            bail!("not used")
        }

        fn detach(&self, _id: &SessionId) -> anyhow::Result<()> {
            Ok(())
        }

        fn kill(&self, _id: &SessionId) -> anyhow::Result<()> {
            Ok(())
        }

        fn status_delivery(&self) -> StatusDelivery {
            StatusDelivery::Push
        }

        fn upgrade_daemon(&self) -> anyhow::Result<DaemonUpgradeOutcome> {
            self.upgrades.fetch_add(1, Ordering::SeqCst);
            Ok(DaemonUpgradeOutcome::Upgraded)
        }
    }

    /// A daemon that mints workspaces and does nothing else.
    ///
    /// Enough for the flow to run its whole decision — ensure, match, create —
    /// and to be counted afterwards. Attaching is what a test window cannot do
    /// for want of a pty, so every flow here ends on the plain terminal, which
    /// is beside the point of what these tests assert.
    #[derive(Default)]
    struct LocalBackend {
        /// Workspace records minted, which is what a connect flow creates.
        creates: AtomicUsize,
        /// What `create_workspace` has minted, so `list_workspaces` reflects
        /// it — same as the real daemon does. A background reconcile pass
        /// (`store.refresh`, fired mid-flow) reads a mismatch here as "no
        /// daemon record" and drops the row it was just handed.
        workspaces: Mutex<Vec<BackendWorkspace>>,
        /// A daemon that cannot be listed, which is what makes an ensure fail.
        unreachable: bool,
        /// A daemon whose failure carries the pre-cut diagnosis, i.e. one this
        /// client cannot speak to.
        incompatible: bool,
        /// A daemon that refuses the handshake with a typed
        /// `unsupported_generation` — the other direction, where the daemon is
        /// the newer end.
        newer: bool,
        /// Deploys this backend was asked for, which a newer daemon must never
        /// be.
        upgrades: AtomicUsize,
        /// Sessions that are gone the moment they are asked about, which is how
        /// a workspace opens dead with nothing installed.
        dead: bool,
        /// Answer the first listing, then meet every later one with
        /// `incompatible`/`newer`: the host's daemon replaced between the
        /// connect flow's ensure and its adoption.
        breaks_after_first_list: bool,
        listings: AtomicUsize,
    }

    impl LocalBackend {
        fn creates(&self) -> usize {
            self.creates.load(Ordering::SeqCst)
        }
    }

    impl SessionBackend for LocalBackend {
        fn create(&self, spec: &SessionSpec) -> anyhow::Result<SessionId> {
            Ok(spec.id.clone())
        }

        fn create_workspace(
            &self,
            root: &std::path::Path,
            name: Option<&str>,
        ) -> anyhow::Result<BackendWorkspace> {
            let minted = self.creates.fetch_add(1, Ordering::SeqCst) + 1;
            let workspace = BackendWorkspace {
                id: format!("ws-{minted}"),
                name: name.unwrap_or_default().to_owned(),
                project_root: root.display().to_string(),
                created_at: 1_700_000_000,
            };
            self.workspaces.lock().unwrap().push(workspace.clone());
            Ok(workspace)
        }

        fn list(&self) -> anyhow::Result<Vec<SessionInfo>> {
            Ok(Vec::new())
        }

        fn list_workspaces(&self) -> anyhow::Result<crate::WorkspaceListing> {
            let listed = self.listings.fetch_add(1, Ordering::SeqCst);
            if self.breaks_after_first_list && listed == 0 {
                return Ok(crate::WorkspaceListing {
                    workspaces: self.workspaces.lock().unwrap().clone(),
                    degraded: false,
                });
            }
            if self.incompatible {
                bail!("{}", ade_session::PRE_CUT_DIAGNOSIS);
            }
            if self.newer {
                return Err(anyhow::Error::new(crate::DaemonRefusal {
                    code: ade_session::error_code::UNSUPPORTED_GENERATION.to_owned(),
                    message: "no protocol generation is common".to_owned(),
                })
                .context("the session daemon refused the handshake"));
            }
            if self.unreachable {
                bail!("no route to the daemon");
            }
            Ok(crate::WorkspaceListing {
                workspaces: self.workspaces.lock().unwrap().clone(),
                degraded: false,
            })
        }

        /// Alive by default, so a reattach does not repair anything and every
        /// `create` counted here is a workspace being made.
        fn exists(&self, _id: &SessionId) -> anyhow::Result<bool> {
            Ok(!self.dead)
        }

        fn attach(&self, _spec: &SessionSpec) -> anyhow::Result<Attached> {
            bail!("a test window has no pty to attach")
        }

        fn detach(&self, _id: &SessionId) -> anyhow::Result<()> {
            Ok(())
        }

        fn kill(&self, _id: &SessionId) -> anyhow::Result<()> {
            Ok(())
        }

        fn status_delivery(&self) -> StatusDelivery {
            StatusDelivery::Push
        }

        fn subscribe_events(&self) -> anyhow::Result<smol::channel::Receiver<DaemonEvent>> {
            let (_sender, receiver) = smol::channel::unbounded();
            Ok(receiver)
        }

        fn upgrade_daemon(&self) -> anyhow::Result<DaemonUpgradeOutcome> {
            self.upgrades.fetch_add(1, Ordering::SeqCst);
            Ok(DaemonUpgradeOutcome::Upgraded)
        }
    }

    /// Plain terminals deployed since this test started.
    fn plain_terminals() -> usize {
        PLAIN_TERMINALS.with(|count| count.get())
    }

    /// The globals a window needs, and a fake filesystem holding `roots`.
    async fn init_test(cx: &mut TestAppContext, roots: &[&str]) -> Arc<fs::FakeFs> {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        for root in roots {
            fs.insert_tree(*root, serde_json::json!({ "README.md": "test" }))
                .await;
        }
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        fs
    }

    /// A window over a local project rooted at `root`.
    async fn open_window(
        fs: &Arc<fs::FakeFs>,
        root: &str,
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, VisualTestContext) {
        let project = project::Project::test(fs.clone(), [Path::new(root)], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        (workspace, cx.clone())
    }

    async fn test_window(cx: &mut TestAppContext) -> (Entity<Workspace>, VisualTestContext) {
        let fs = init_test(cx, &["/repo"]).await;
        open_window(&fs, "/repo", cx).await
    }

    /// Puts that backend behind the process-wide lifecycle service, which is
    /// where the flow takes its own from.
    async fn install_lifecycle(
        name: &'static str,
        backend: Arc<LocalBackend>,
        cx: &mut TestAppContext,
    ) -> Arc<WorkspaceLifecycleService> {
        let registry = AdeWorkspaceRegistry::open_test_db(name).await;
        let service = Arc::new(WorkspaceLifecycleService::with_backend(registry, backend));
        cx.update(|cx| cx.set_global(GlobalLifecycleService(service.clone())));
        service
    }

    fn run_flow(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                open_connection_workspace(workspace, window, cx)
            })
        })
    }

    fn is_claimed(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
        cx.update(|_, cx| {
            cx.default_global::<ClaimedWindows>()
                .0
                .contains(&workspace.entity_id())
        })
    }

    fn roots(lifecycle: &WorkspaceLifecycleService) -> Vec<(PathBuf, Option<String>)> {
        lifecycle
            .registry()
            .list_workspaces()
            .expect("the registry lists")
            .into_iter()
            .map(|workspace| (workspace.repository_path, workspace.remote_host))
            .collect()
    }

    fn ssh(host: &str, username: Option<&str>, port: Option<u16>) -> SshConnectionOptions {
        SshConnectionOptions {
            host: host.to_owned().into(),
            username: username.map(str::to_owned),
            port,
            ..Default::default()
        }
    }

    #[test]
    fn test_destination_round_trips_through_parse() {
        for options in [
            ssh("wsl-box", None, None),
            ssh("wsl-box", Some("kingii"), None),
            ssh("wsl-box", Some("kingii"), Some(2222)),
            ssh("::1", Some("root"), Some(2222)),
        ] {
            let destination = ssh_destination(&options);
            assert!(
                host_matches_destination(&options, Some(&destination)),
                "options did not match their own destination {destination:?}"
            );
        }
    }

    #[test]
    fn test_a_destination_with_fewer_parts_still_matches() {
        // Stored as a bare host; the window connects with a username and the
        // default port. The parts the destination does not state are lenient.
        assert!(host_matches_destination(
            &ssh("wsl-box", Some("kingii"), Some(22)),
            Some("wsl-box")
        ));
        // But a stated part must agree.
        assert!(!host_matches_destination(
            &ssh("wsl-box", Some("kingii"), None),
            Some("ada@wsl-box")
        ));
        assert!(!host_matches_destination(
            &ssh("other-box", None, None),
            Some("wsl-box")
        ));
    }

    #[test]
    fn test_the_home_directory_is_not_a_workspace() {
        let with_user = ssh("wsl-box", Some("kingii"), None);
        assert!(is_remote_home_directory(
            Path::new("/home/kingii"),
            &with_user
        ));
        assert!(!is_remote_home_directory(
            Path::new("/home/kingii/testproj"),
            &with_user
        ));
        // No username stated: ssh would resolve it, guessing would refuse a
        // real project, so nothing is refused.
        assert!(!is_remote_home_directory(
            Path::new("/home/kingii"),
            &ssh("wsl-box", None, None)
        ));
    }

    /// Everything a candidate — row or discovered record — has to agree with
    /// this window on. Which of the survivors is opened is the lifecycle
    /// layer's rule, not this predicate's.
    fn matching(host: &WindowDaemonHost, root: &str, candidates: &[AdeWorkspace]) -> Vec<String> {
        candidates
            .iter()
            .filter(|workspace| {
                host.holds(
                    workspace.remote_host.as_deref(),
                    &workspace.repository_path,
                    Path::new(root),
                )
            })
            .map(|workspace| workspace.name.clone())
            .collect()
    }

    #[test]
    fn test_a_candidate_needs_the_exact_root() {
        let host = WindowDaemonHost::Ssh(ssh("wsl-box", Some("kingii"), None));
        let mut exact = AdeWorkspace::new("main", "testproj", "/home/kingii/testproj");
        exact.remote_host = Some("wsl-box".to_owned());
        // The same host spelled with its username: still this window's.
        let mut inside_new = AdeWorkspace::new("new", "testproj", "/home/kingii/testproj/wt");
        inside_new.remote_host = Some("kingii@wsl-box".to_owned());
        let mut elsewhere = AdeWorkspace::new("elsewhere", "other", "/home/kingii/other");
        elsewhere.remote_host = Some("wsl-box".to_owned());
        let mut other_host = AdeWorkspace::new("other-host", "testproj", "/home/kingii/testproj");
        other_host.remote_host = Some("gpu-box".to_owned());

        let candidates = vec![exact, inside_new, elsewhere, other_host];
        assert_eq!(
            matching(&host, "/home/kingii/testproj", &candidates),
            vec!["main"]
        );
        assert_eq!(
            matching(&host, "/home/kingii/testproj/wt", &candidates),
            vec!["new"]
        );
    }

    #[test]
    fn test_only_wsl_and_docker_windows_are_refused() {
        assert!(matches!(
            WindowDaemonHost::for_connection(None),
            Some(WindowDaemonHost::Local)
        ));
        assert!(matches!(
            WindowDaemonHost::for_connection(Some(RemoteConnectionOptions::Ssh(ssh(
                "wsl-box", None, None
            )))),
            Some(WindowDaemonHost::Ssh(_))
        ));
        assert!(
            WindowDaemonHost::for_connection(Some(RemoteConnectionOptions::Wsl(
                WslConnectionOptions {
                    distro_name: "Ubuntu".to_owned(),
                    user: None,
                }
            )))
            .is_none()
        );
        assert!(
            WindowDaemonHost::for_connection(Some(RemoteConnectionOptions::Docker(
                DockerConnectionOptions::default()
            )))
            .is_none()
        );
    }

    #[test]
    fn test_a_local_window_matches_only_local_rows() {
        let local = AdeWorkspace::new("local", "repo", "/repo");
        let mut remote = AdeWorkspace::new("remote", "repo", "/repo");
        remote.remote_host = Some("wsl-box".to_owned());
        let elsewhere = AdeWorkspace::new("elsewhere", "other", "/other");
        let candidates = vec![local, remote, elsewhere];

        assert_eq!(
            matching(&WindowDaemonHost::Local, "/repo", &candidates),
            vec!["local"]
        );
        // The same path on a host is a different project, and picking it would
        // attach this window to sessions on a machine it is not connected to.
        let host = WindowDaemonHost::Ssh(ssh("wsl-box", None, None));
        assert_eq!(matching(&host, "/repo", &candidates), vec!["remote"]);
    }

    /// Windows spells the same directory several ways; the flow must not mint a
    /// second workspace for one it already has.
    #[cfg(target_os = "windows")]
    #[test]
    fn test_a_local_root_matches_whatever_case_the_os_spelled_it() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let root = directory.path();
        let shouted = PathBuf::from(root.to_string_lossy().to_uppercase());
        assert_ne!(root, shouted.as_path(), "the paths must differ as written");
        assert!(same_local_path(root, &shouted));
        // A path that does not resolve is compared as written, so nothing
        // collapses two real projects into one.
        assert!(!same_local_path(root, &root.join("child")));
    }

    #[gpui::test]
    async fn test_a_local_window_adopts_its_project(cx: &mut TestAppContext) {
        let backend = Arc::new(LocalBackend::default());
        let fs = init_test(cx, &["/repo"]).await;
        let lifecycle = install_lifecycle("connect_local_adopts", backend.clone(), cx).await;
        let (first, mut first_window) = open_window(&fs, "/repo", cx).await;

        assert!(run_flow(&first, &mut first_window));
        first_window.run_until_parked();
        assert_eq!(roots(&lifecycle), vec![(PathBuf::from("/repo"), None)]);
        assert_eq!(backend.creates(), 1);
        // The attach failed for want of a pty, so the window went back. Keeping
        // the claim is the *success* outcome, and success needs a real terminal
        // — not reachable here, and not worth faking.
        assert!(!is_claimed(&first, &mut first_window));

        // A second window on the same root reattaches to that workspace rather
        // than making a second one.
        let (second, mut second_window) = open_window(&fs, "/repo", cx).await;
        assert!(run_flow(&second, &mut second_window));
        second_window.run_until_parked();
        assert_eq!(roots(&lifecycle), vec![(PathBuf::from("/repo"), None)]);
        assert_eq!(backend.creates(), 1);
    }

    /// Guards the outcome, not the lock: the test registry runs its writes
    /// synchronously, so the decision phase cannot actually be interleaved
    /// here. The lock itself is exercised in `lifecycle.rs`, which can hold it.
    #[gpui::test]
    async fn test_two_windows_on_one_root_create_one_workspace(cx: &mut TestAppContext) {
        let backend = Arc::new(LocalBackend::default());
        let fs = init_test(cx, &["/repo"]).await;
        let lifecycle = install_lifecycle("connect_local_race", backend.clone(), cx).await;
        let (first, mut first_window) = open_window(&fs, "/repo", cx).await;
        let (second, mut second_window) = open_window(&fs, "/repo", cx).await;

        // Both claims land before either flow gets to look at the registry,
        // which is the race the decision lock exists for.
        assert!(run_flow(&first, &mut first_window));
        assert!(run_flow(&second, &mut second_window));
        first_window.run_until_parked();
        second_window.run_until_parked();

        assert_eq!(roots(&lifecycle), vec![(PathBuf::from("/repo"), None)]);
        assert_eq!(backend.creates(), 1);
    }

    #[gpui::test]
    async fn test_a_local_window_rooted_at_home_is_not_adopted(cx: &mut TestAppContext) {
        let backend = Arc::new(LocalBackend::default());
        let home = util::paths::home_dir().to_string_lossy().into_owned();
        let fs = init_test(cx, &[home.as_str()]).await;
        let lifecycle = install_lifecycle("connect_local_home", backend.clone(), cx).await;
        let (workspace, mut window) = open_window(&fs, home.as_str(), cx).await;

        assert!(run_flow(&workspace, &mut window));
        window.run_until_parked();

        assert_eq!(roots(&lifecycle), Vec::new());
        assert_eq!(backend.creates(), 0);
        assert!(
            !is_claimed(&workspace, &mut window),
            "a window that fell back must be free to run the flow again"
        );
    }

    #[gpui::test]
    async fn test_an_unreachable_daemon_leaves_the_window_retryable(cx: &mut TestAppContext) {
        let backend = Arc::new(LocalBackend {
            unreachable: true,
            ..Default::default()
        });
        let fs = init_test(cx, &["/repo"]).await;
        let lifecycle = install_lifecycle("connect_local_unreachable", backend.clone(), cx).await;
        let (workspace, mut window) = open_window(&fs, "/repo", cx).await;

        assert!(run_flow(&workspace, &mut window));
        window.run_until_parked();

        assert_eq!(roots(&lifecycle), Vec::new());
        assert!(!is_claimed(&workspace, &mut window));
        // The retry runs the whole flow again rather than reporting the window
        // as already taken.
        assert!(run_flow(&workspace, &mut window));
        assert!(is_claimed(&workspace, &mut window));
    }

    /// An incompatible daemon on this machine is refused once, not on every
    /// workspace switch and terminal click for the rest of the session.
    #[gpui::test]
    async fn test_an_incompatible_local_daemon_is_refused_for_the_session(cx: &mut TestAppContext) {
        let backend = Arc::new(LocalBackend {
            incompatible: true,
            ..Default::default()
        });
        let fs = init_test(cx, &["/repo"]).await;
        let lifecycle = install_lifecycle("connect_local_incompatible", backend.clone(), cx).await;
        let (workspace, mut window) = open_window(&fs, "/repo", cx).await;
        let fallbacks = plain_terminals();

        assert!(run_flow(&workspace, &mut window));
        window.run_until_parked();
        assert_eq!(roots(&lifecycle), Vec::new());
        assert!(!is_claimed(&workspace, &mut window));
        assert!(window.update(|_, cx| cx.default_global::<RefusedDaemons>().0.contains(&None)));
        // §6.1: that daemon may still own a live agent's writer, so ADE is
        // left unresolved rather than given a competing shell.
        assert_eq!(plain_terminals(), fallbacks);

        // The next trigger takes no claim at all, so nothing ensures and no
        // second error reaches the user.
        assert!(!run_flow(&workspace, &mut window));
        assert!(!is_claimed(&workspace, &mut window));
    }

    /// **An incompatibility found after the first listing is the same
    /// incompatibility.** The host's daemon is replaced in the gap between the
    /// connect flow's ensure and its adoption — another client's forced
    /// upgrade, an operator — and §6.1 does not care which phase met it: the
    /// window is left unresolved, never given a competing shell. Run over both
    /// shapes (pre-cut EOF and typed refusal) and both registry states, because
    /// a cached row used to answer for the daemon that was gone.
    #[gpui::test]
    async fn test_an_incompatibility_at_adoption_never_opens_a_terminal(cx: &mut TestAppContext) {
        for (case, newer, cached_row) in [
            ("precut_fresh", false, false),
            ("precut_cached", false, true),
            ("refusal_fresh", true, false),
            ("refusal_cached", true, true),
        ] {
            let backend = Arc::new(LocalBackend {
                breaks_after_first_list: true,
                incompatible: !newer,
                newer,
                ..Default::default()
            });
            let fs = init_test(cx, &["/repo"]).await;
            let lifecycle = install_lifecycle(
                match case {
                    "precut_fresh" => "connect_break_precut_fresh",
                    "precut_cached" => "connect_break_precut_cached",
                    "refusal_fresh" => "connect_break_refusal_fresh",
                    _ => "connect_break_refusal_cached",
                },
                backend.clone(),
                cx,
            )
            .await;
            if cached_row {
                let mut row = AdeWorkspace::new("repo", "repo", "/repo");
                row.terminal_session_id = Some("ws-1".to_owned());
                lifecycle
                    .registry()
                    .create_workspace(row)
                    .await
                    .expect("seeding the cached row");
            }
            let (workspace, mut window) = open_window(&fs, "/repo", cx).await;
            let fallbacks = plain_terminals();
            // One app across the four cases: the previous case's standing
            // refusal would otherwise stop this one's flow before it starts.
            window.update(|_, cx| cx.default_global::<RefusedDaemons>().0.clear());

            assert!(run_flow(&workspace, &mut window));
            window.run_until_parked();

            assert!(
                backend.listings.load(Ordering::SeqCst) > 1,
                "{case}: the ensure succeeded, so this is the adoption phase failing"
            );
            assert_eq!(plain_terminals(), fallbacks, "{case}: §6.1 forbids a shell");
            assert_eq!(backend.creates(), 0, "{case}");
            assert!(!is_claimed(&workspace, &mut window), "{case}");
            // Local host, so there is nothing to offer replacing: the user is
            // told, and the daemon is left alone for the rest of the session.
            assert!(
                window.update(|_, cx| cx.default_global::<RefusedDaemons>().0.contains(&None)),
                "{case}: the user was shown the incompatibility"
            );
        }
    }

    /// The other direction: a daemon newer than this client. It is refused the
    /// same way, and **nothing offers to replace it** — that would be a
    /// downgrade over its live sessions.
    #[gpui::test]
    async fn test_a_newer_daemon_is_refused_without_any_deploy(cx: &mut TestAppContext) {
        let backend = Arc::new(LocalBackend {
            newer: true,
            ..Default::default()
        });
        let fs = init_test(cx, &["/repo"]).await;
        let lifecycle = install_lifecycle("connect_newer_daemon", backend.clone(), cx).await;
        let (workspace, mut window) = open_window(&fs, "/repo", cx).await;
        let fallbacks = plain_terminals();

        assert!(run_flow(&workspace, &mut window));
        window.run_until_parked();

        assert_eq!(roots(&lifecycle), Vec::new());
        assert_eq!(backend.upgrades.load(Ordering::SeqCst), 0);
        assert_eq!(plain_terminals(), fallbacks);
        assert!(!is_claimed(&workspace, &mut window));
    }

    /// The one rule the dialog must never get wrong, over every combination it
    /// can be reached with.
    #[test]
    fn test_only_an_older_remote_daemon_may_be_replaced() {
        assert_eq!(
            upgradable_host(Outdated::Daemon, Some("fevm1".to_owned())),
            Some("fevm1".to_owned())
        );
        assert_eq!(upgradable_host(Outdated::Daemon, None), None);
        assert_eq!(
            upgradable_host(Outdated::Client, Some("fevm1".to_owned())),
            None,
            "deploying this client's binary over a newer daemon is a downgrade"
        );
        assert_eq!(upgradable_host(Outdated::Client, None), None);
    }

    /// An ordinary reachability failure keeps the fallback it always had: the
    /// window owes the user a shell, and no daemon is holding one.
    #[gpui::test]
    async fn test_an_unreachable_daemon_still_gets_a_plain_terminal(cx: &mut TestAppContext) {
        let backend = Arc::new(LocalBackend {
            unreachable: true,
            ..Default::default()
        });
        let fs = init_test(cx, &["/repo"]).await;
        install_lifecycle("connect_unreachable_terminal", backend.clone(), cx).await;
        let (workspace, mut window) = open_window(&fs, "/repo", cx).await;
        let fallbacks = plain_terminals();

        assert!(run_flow(&workspace, &mut window));
        window.run_until_parked();

        assert_eq!(plain_terminals(), fallbacks + 1);
    }

    /// Opening can succeed having installed nothing: a workspace whose session
    /// re-probes dead parks on the panel's "gone" affordance. The claim must
    /// not stay on a window ADE does not own, or every later trigger sees it
    /// and does nothing.
    #[gpui::test]
    async fn test_a_workspace_that_opens_dead_hands_the_window_back(cx: &mut TestAppContext) {
        let backend = Arc::new(LocalBackend {
            dead: true,
            ..Default::default()
        });
        let fs = init_test(cx, &["/repo"]).await;
        let lifecycle = install_lifecycle("connect_local_opens_dead", backend.clone(), cx).await;
        let (workspace, mut window) = open_window(&fs, "/repo", cx).await;

        assert!(run_flow(&workspace, &mut window));
        window.run_until_parked();

        assert_eq!(roots(&lifecycle), vec![(PathBuf::from("/repo"), None)]);
        assert!(!workspace.read_with(&window, |workspace, _| workspace.ade_owns_layout()));
        assert!(!is_claimed(&workspace, &mut window));
    }

    #[gpui::test]
    async fn test_releasing_a_claim_allows_the_window_to_reconnect(cx: &mut TestAppContext) {
        let (workspace, mut window) = test_window(cx).await;
        window.update(|_, cx| {
            let id = workspace.entity_id();
            assert!(cx.default_global::<ClaimedWindows>().0.insert(id));
            release_window_claim(id, cx);
            assert!(cx.default_global::<ClaimedWindows>().0.insert(id));
        });
    }

    #[gpui::test]
    async fn test_incompatible_daemon_requires_confirmation_before_upgrade(
        cx: &mut TestAppContext,
    ) {
        let backend = Arc::new(UpgradeBackend::default());
        let registry =
            crate::AdeWorkspaceRegistry::open_test_db("connect_incompatible_daemon").await;
        let lifecycle = Arc::new(
            WorkspaceLifecycleService::with_backend(registry, backend.clone())
                .with_backend_for_host("fevm1", backend.clone()),
        );
        let (_, mut window) = test_window(cx).await;
        let task = window.update(|window, cx| {
            window.spawn(cx, async move |cx| {
                offer_incompatible_daemon_upgrade("fevm1", lifecycle, cx).await
            })
        });

        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        let (message, detail) = cx.pending_prompt().expect("the incompatibility prompt");
        assert!(message.contains("incompatible"), "{message}");
        assert!(detail.contains("terminate"), "{detail}");
        assert_eq!(backend.upgrades.load(Ordering::SeqCst), 0);

        cx.simulate_prompt_answer("Terminate sessions and upgrade");
        assert!(task.await.expect("the upgrade prompt succeeds"));
        assert_eq!(backend.upgrades.load(Ordering::SeqCst), 1);
    }
}
