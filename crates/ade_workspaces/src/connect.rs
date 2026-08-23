//! A fresh window — local or a fresh ssh connection — reattaches to a daemon
//! workspace.
//!
//! The first window on a fresh connection used to open a plain centre-pane
//! shell that died with the window, so connecting twice (or just closing and
//! reopening Zorca) left throwaway shells and nothing to come back to. This
//! module is the other branch of that fresh-window decision: it asks the right
//! daemon — this machine's or the ssh host's — what it already holds, and then
//! reattaches to the project's most recently opened workspace or creates the
//! first. A workspace only the daemon knows stays available in the sidebar for
//! the user to open instead of being duplicated here.
//!
//! Opening attaches: it builds the daemon's layout and attaches to the
//! sessions it names, so a second connect reattaches rather than spawning, and
//! a session that died with its host still surfaces as gone.
//!
//! **WSL and Docker keep the plain terminal.** ADE's session layer takes an
//! ssh destination or nothing at all, and those transports have neither. (A
//! WSL box reached *over ssh* is an ssh host, and takes the workspace path.)
//! A host that cannot be reached has answered nothing and keeps the plain
//! terminal too, with a log line: a connection must never cost the user their
//! shell. **Local on a platform without a local daemon** (see
//! [`WorkspaceLifecycleService::new`]) keeps the plain terminal for the same
//! reason.

use crate::{
    AdeWorkspace, SessionState, WorkspaceEntry, WorkspaceLifecycleService,
    attach::attach_terminal,
    open_workspace_session,
    store::AdeWorkspaceStore,
    workspace_view::{ensure_repository_worktree, name_window_after_workspace},
};
use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, EntityId, Global, PromptLevel, WeakEntity,
    Window,
};
use remote::{RemoteConnectionOptions, SshConnectionOptions};
use std::collections::HashSet;
use std::path::Path;
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

/// `true` when this window is now the caller's to run the flow in.
fn claim_window(window: EntityId, cx: &mut App) -> bool {
    cx.default_global::<ClaimedWindows>().0.insert(window)
}

pub(crate) fn release_window_claim(window: EntityId, cx: &mut App) {
    if cx.has_global::<ClaimedWindows>() {
        cx.global_mut::<ClaimedWindows>().0.remove(&window);
    }
}

/// What a window the flow gave up waiting on gets: its shell, and its claim
/// back.
///
/// The empty window's deadline is short because a fresh window owes the user a
/// prompt, which makes it a guess rather than an answer — a project the user
/// just added can take longer than that to grow its first worktree. Keeping the
/// claim would settle the guess for good: the flow runs once per window, so no
/// later add or activation could try again with the root that has since landed,
/// and the window would keep its stock non-ADE terminal permanently. Handing the
/// claim back is what makes the next attempt possible.
///
/// The terminal still lands, and only into a window that is still empty: a
/// rootless connection is owed a prompt either way, and the next attempt finds
/// a workspace with no stored layout, so it attaches a tab beside that shell
/// rather than rebuilding the window around it.
fn give_up_on_window(window: EntityId, this: &WeakEntity<Workspace>, cx: &mut AsyncWindowContext) {
    cx.update(|_, cx| release_window_claim(window, cx)).ok();
    open_plain_terminal_if_empty(this, cx);
}

/// OpenSSH's own default, and what `~/.ssh/config` resolution assumes when the
/// settings leave the port unset.
const DEFAULT_SSH_PORT: u16 = 22;

/// Which connection a fresh window is, for the part of the decision that does
/// not need a live project: `Some(Some(ssh))` is an ssh connection,
/// `Some(None)` is local, and `None` is WSL, Docker, or any other connection
/// kind — none of ADE's session layer, which speaks ssh or nothing at all.
///
/// Local is `Some(None)` only on unix: `backend_for_host` knows a local daemon
/// there and nowhere else (see `WorkspaceLifecycleService::new`), so a local
/// window elsewhere stays the plain terminal it always was.
fn ade_connection(
    options: Option<RemoteConnectionOptions>,
) -> Option<Option<SshConnectionOptions>> {
    match options {
        Some(RemoteConnectionOptions::Ssh(ssh)) => Some(Some(ssh)),
        None if cfg!(unix) => Some(None),
        _ => None,
    }
}

/// Takes over a fresh window that is local or an ssh connection: `true` means
/// this window now belongs to the connect flow (which opens a workspace, or
/// falls back to a plain terminal itself), `false` means the window is not
/// ADE's to take — WSL, Docker, a platform with no local daemon, or a
/// connection rooted at the connected account's home — and the caller should
/// open whatever a fresh window normally gets.
pub fn open_connection_workspace(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let project = workspace.project().clone();
    let Some(ssh) = ade_connection(project.read(cx).remote_connection_options(cx)) else {
        return false;
    };
    // A fake fs has no local machine or daemon behind it. SSH is unaffected:
    // its daemon lives on the host.
    if ssh.is_none() && project.read(cx).fs().is_fake() {
        return false;
    }
    // The destination a workspace's `remote_host` column records — `None` is
    // this machine, exactly as `WorkspaceLifecycleService::create_workspace`
    // already defines it.
    let host = ssh.as_ref().map(ssh_destination);
    let label = host.clone().unwrap_or_else(|| "this machine".to_owned());
    let window_id = cx.entity().entity_id();
    if !claim_window(window_id, cx) {
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
    // fast, and a connection opened with no folder at all never grows a
    // worktree, which the short deadline turns into the plain terminal.
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
        let Some(project_scope) = wait_for_project_root(&this, root_deadline, cx).await else {
            log::info!(
                "ADE waited {root_deadline:?} for {label}'s project root; \
                 releasing the claim so a later caller can try again"
            );
            give_up_on_window(window_id, &this, cx);
            return;
        };
        let repository_path = project_scope.repository_path;
        let project_id = project_scope.project_id;
        let project_identity = project_scope.project_identity;
        // Connecting with nothing but `~` filled in is how the remote picker
        // behaves before a folder is chosen; a workspace rooted at the whole
        // account is not a project (operator ruling, 2026-08-05). The same
        // ruling applies to a local window rooted at the user's own home.
        let is_home = match &ssh {
            Some(ssh) => is_remote_home_directory(&repository_path, ssh),
            None => repository_path == *util::paths::home_dir(),
        };
        if is_home {
            open_plain_terminal_if_empty(&this, cx);
            return;
        }

        // Blocking, deliberately: the ensure drives the host's ssh connection
        // (or the local daemon proxy) and the listing reads the daemon, then
        // sqlite.
        let listed = cx
            .background_spawn({
                let lifecycle = lifecycle.clone();
                let host = host.clone();
                async move { lifecycle.ensure_host_workspaces(host.as_deref()).await }
            })
            .await;

        let listed = match listed {
            // Upgrading is remote-only (`upgrade_host_daemon` always targets a
            // named host): the local daemon is this same client's own binary,
            // so it cannot fall out of protocol sync with itself, and nothing
            // here may offer to stop or upgrade it.
            Err(error) if host.is_some() && crate::daemon_backend::is_incompatible_daemon(&error) => {
                log::warn!(
                    "ADE found an incompatible session daemon on {label}: {error:#}"
                );
                match offer_incompatible_daemon_upgrade(&label, lifecycle.clone(), cx).await {
                    Ok(true) => {
                        cx.background_spawn({
                            let lifecycle = lifecycle.clone();
                            let host = host.clone();
                            async move {
                                lifecycle.ensure_host_workspaces(host.as_deref()).await
                            }
                        })
                        .await
                    }
                    Ok(false) => return,
                    Err(error) => {
                        log::warn!("upgrading the session daemon on {label} failed: {error:#}");
                        let detail = format!("{error:#}");
                        match cx.update(|window, cx| {
                            window.prompt(
                                PromptLevel::Critical,
                                "Failed to upgrade the remote session daemon",
                                Some(&detail),
                                &["OK"],
                                cx,
                            )
                        }) {
                            Ok(prompt) => {
                                if let Err(prompt_error) = prompt.await {
                                    log::warn!(
                                        "showing the daemon upgrade error failed: {prompt_error:#}"
                                    );
                                }
                            }
                            Err(prompt_error) => {
                                log::warn!(
                                    "showing the daemon upgrade error failed: {prompt_error:#}"
                                );
                            }
                        }
                        return;
                    }
                }
            }
            listed => listed,
        };

        let workspaces = match listed {
            Ok(workspaces) => workspaces,
            Err(error) => {
                log::warn!(
                    "ADE could not reach {label}, so this connection gets a plain terminal: {error:#}"
                );
                open_plain_terminal_if_empty(&this, cx);
                return;
            }
        };

        // A row this client already has for the root leads. A transient record
        // stays in the sidebar until the user opens it; creating beside it
        // would duplicate the daemon workspace.
        let existing_id = most_recent_workspace(ssh.as_ref(), &repository_path, &workspaces)
            .map(|workspace| workspace.id.clone());
        let persisted_ids = workspaces
            .iter()
            .filter_map(WorkspaceEntry::persisted)
            .map(|(workspace, _)| workspace)
            .filter(|workspace| {
                persisted_workspace_matches_connection(ssh.as_ref(), workspace)
                    && workspace.repository_path == repository_path
            })
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>();
        let discovered_ids = workspaces
            .iter()
            .filter_map(|entry| match entry {
                WorkspaceEntry::Discovered {
                    remote_host,
                    workspace,
                    ..
                } if workspace_matches_connection(ssh.as_ref(), remote_host.as_deref())
                    && Path::new(&workspace.project_root) == repository_path =>
                {
                    Some(workspace.id.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let scoped = cx
            .background_spawn({
                let lifecycle = lifecycle.clone();
                let host = host.clone();
                let project_id = project_id.clone();
                let project_identity = project_identity.clone();
                async move {
                    let mut persisted = Vec::with_capacity(persisted_ids.len());
                    for id in persisted_ids {
                        persisted.push(
                            lifecycle
                                .update_workspace_project_scope(
                                    &id,
                                    &project_id,
                                    &project_identity,
                                )
                                .await?,
                        );
                    }
                    let mut discovered_exists = false;
                    for id in discovered_ids {
                        discovered_exists |= lifecycle
                            .update_discovered_workspace_project_scope(
                                host.as_deref(),
                                &id,
                                &project_id,
                                &project_identity,
                            )
                            .await?
                            .is_some();
                    }
                    anyhow::Ok((persisted, discovered_exists))
                }
            })
            .await;
        let (persisted, discovered_exists) = match scoped {
            Ok(scoped) => scoped,
            Err(error) => {
                log::warn!("updating ADE project identity for {label} failed: {error:#}");
                open_plain_terminal_if_empty(&this, cx);
                return;
            }
        };

        cx.update(|_, cx| {
            if let Some(store) = AdeWorkspaceStore::try_global(cx) {
                store.update(cx, |store, cx| store.refresh(cx));
            }
        })
        .ok();

        let existing = existing_id
            .as_ref()
            .and_then(|id| persisted.iter().find(|workspace| &workspace.id == id))
            .cloned();
        if existing.is_none() && discovered_exists {
            open_plain_terminal_if_empty(&this, cx);
            return;
        }
        let opened = match existing {
            Some(existing) => open_or_recreate(&this, &lifecycle, existing, cx).await,
            None => {
                let created = cx
                    .background_spawn({
                        let lifecycle = lifecycle.clone();
                        let name = project_id.clone();
                        let repository_path = repository_path.clone();
                        let host = host.clone();
                        async move {
                            lifecycle
                                .create_workspace_scoped(
                                    name,
                                    project_id,
                                    Some(project_identity),
                                    repository_path,
                                    None,
                                    host,
                                )
                                .await
                        }
                    })
                    .await;
                match created {
                    Ok(created) => open_in_window(&this, created.id, cx).await,
                    Err(error) => Err(error),
                }
            }
        };

        if let Err(error) = opened {
            // The window must not come up empty: whatever the workspace path
            // could not deliver, the connection still owes the user a shell.
            log::warn!("opening the ADE workspace for {label} failed: {error:#}");
            open_plain_terminal_if_empty(&this, cx);
        }
    })
    .detach();
    true
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
struct ProjectScope {
    repository_path: std::path::PathBuf,
    project_id: String,
    project_identity: String,
}

async fn wait_for_project_root(
    this: &WeakEntity<Workspace>,
    deadline: std::time::Duration,
    cx: &mut AsyncWindowContext,
) -> Option<ProjectScope> {
    let poll = std::time::Duration::from_millis(250);
    let mut waited = std::time::Duration::ZERO;
    loop {
        let scope = this
            .update(cx, |workspace, cx| {
                let project = workspace.project().read(cx);
                let repository_path = project
                    .visible_worktrees(cx)
                    .next()
                    .map(|worktree| worktree.read(cx).abs_path().to_path_buf())?;
                if !workspace.project_group_identity_is_known(cx) {
                    return None;
                }
                let project_group_key = project.project_group_key(cx);
                let project_identity = project_group_key.path_list().serialize().paths;
                if project_identity.is_empty() {
                    return None;
                }
                Some(ProjectScope {
                    repository_path,
                    project_id: project_group_key
                        .display_name(&Default::default())
                        .to_string(),
                    project_identity,
                })
            })
            .ok()?;
        if let Some(scope) = scope {
            return Some(scope);
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

/// Opens the workspace — recreating its session first if everything in it is
/// dead.
///
/// Opening a dead workspace normally parks on the panel's "gone" row and its
/// Recreate button, which is right for a *click*: surfacing a crash beats
/// quietly papering over it. A **connection** is different — it is the user
/// explicitly asking for a live shell on this host, and honoring it with a
/// dead row and no working terminal serves nobody. So the connect flow is the
/// second caller of the panel's own repair: recreate, then attach, exactly as
/// the Recreate button does.
async fn open_or_recreate(
    this: &WeakEntity<Workspace>,
    lifecycle: &std::sync::Arc<WorkspaceLifecycleService>,
    existing: AdeWorkspace,
    cx: &mut AsyncWindowContext,
) -> anyhow::Result<()> {
    let id = existing.id.clone();
    let probed = cx
        .background_spawn({
            let lifecycle = lifecycle.clone();
            let id = id.clone();
            async move { lifecycle.open_workspace(&id).await }
        })
        .await?;
    let (_, state) = probed;
    if !matches!(state, SessionState::Dead) {
        return open_in_window(this, id, cx).await;
    }

    log::info!(
        "workspace {} has no live session; recreating one for this connection",
        id
    );
    let (workspace, attached) = cx
        .background_spawn({
            let lifecycle = lifecycle.clone();
            let id = id.clone();
            async move {
                let workspace = lifecycle.recreate_session(&id).await?;
                let attached = lifecycle.attach_command(&workspace)?;
                anyhow::Ok((workspace, attached))
            }
        })
        .await?;
    ensure_repository_worktree(this, &workspace, cx).await;
    name_window_after_workspace(this, &workspace, cx)?;
    attach_terminal(this, &workspace, attached, cx).await
}

/// What a window gets when ADE cannot give it a session: the same centre-pane
/// terminal a fresh, empty window opens with, and one that dies with the
/// window. Only into an **empty** window — a restored layout already gives the
/// user their shells, and stacking one more on top of it would be noise.
fn open_plain_terminal_if_empty(this: &WeakEntity<Workspace>, cx: &mut AsyncWindowContext) {
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

/// The row this connection reattaches to: the most recently opened one for the
/// connected root on this host. `None` means this client has never opened a
/// workspace here, which is what checks [`has_discovered_workspace`].
///
/// `ssh` is `None` for the local connection, and matches only workspaces
/// with no `remote_host` of their own — a local project must never reattach
/// to a row a remote host created.
fn most_recent_workspace<'a>(
    ssh: Option<&SshConnectionOptions>,
    root: &Path,
    entries: &'a [WorkspaceEntry],
) -> Option<&'a AdeWorkspace> {
    entries
        .iter()
        .filter_map(WorkspaceEntry::persisted)
        .map(|(workspace, _)| workspace)
        .filter(|workspace| {
            persisted_workspace_matches_connection(ssh, workspace)
                && workspace.repository_path == root
        })
        // A tie keeps the earlier candidate, so a caller whose list is already
        // ordered — the registry's is `last_opened_at DESC` — gets its own
        // order back rather than an arbitrary one.
        .fold(None, |best: Option<&AdeWorkspace>, workspace| match best {
            Some(best) if best.last_opened_at >= workspace.last_opened_at => Some(best),
            _ => Some(workspace),
        })
}

fn persisted_workspace_matches_connection(
    ssh: Option<&SshConnectionOptions>,
    workspace: &AdeWorkspace,
) -> bool {
    match ssh {
        Some(ssh) if workspace.daemon_id.is_none() => {
            workspace.remote_host.as_deref() == Some(ssh_destination(ssh).as_str())
        }
        Some(ssh) => host_matches_destination(ssh, workspace.remote_host.as_deref()),
        None => workspace.remote_host.is_none(),
    }
}

/// Whether the host already holds a record for this connected root. The
/// sidebar owns promotion; the connection flow only avoids duplicating it.
#[cfg(test)]
fn has_discovered_workspace(
    ssh: Option<&SshConnectionOptions>,
    root: &Path,
    entries: &[WorkspaceEntry],
) -> bool {
    entries.iter().any(|entry| match entry {
        WorkspaceEntry::Discovered {
            remote_host,
            workspace,
            ..
        } if workspace_matches_connection(ssh, remote_host.as_deref())
            && Path::new(&workspace.project_root) == root =>
        {
            true
        }
        _ => false,
    })
}

/// Whether a workspace's stored `remote_host` names the connection this window
/// is on. Local (`ssh` is `None`) matches only a workspace with no
/// `remote_host` of its own; an ssh connection defers to
/// [`host_matches_destination`]'s lenient parts comparison.
fn workspace_matches_connection(
    ssh: Option<&SshConnectionOptions>,
    destination: Option<&str>,
) -> bool {
    match ssh {
        Some(ssh) => host_matches_destination(ssh, destination),
        None => destination.is_none(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Attached, DaemonUpgradeOutcome, SessionBackend, SessionId, SessionInfo, SessionSpec,
        StatusDelivery,
    };
    use anyhow::bail;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use remote::{DockerConnectionOptions, WslConnectionOptions};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Default)]
    struct UpgradeBackend {
        upgrades: AtomicUsize,
    }

    impl SessionBackend for UpgradeBackend {
        fn create(
            &self,
            _spec: &SessionSpec,
            _expected: Option<&str>,
        ) -> anyhow::Result<SessionId> {
            bail!("not used")
        }

        fn list(&self) -> anyhow::Result<Vec<SessionInfo>> {
            Ok(Vec::new())
        }

        fn exists(&self, _id: &SessionId, _expected: Option<&str>) -> anyhow::Result<bool> {
            Ok(false)
        }

        fn attach(&self, _spec: &SessionSpec, _expected: Option<&str>) -> anyhow::Result<Attached> {
            bail!("not used")
        }

        fn detach(&self, _id: &SessionId) -> anyhow::Result<()> {
            Ok(())
        }

        fn kill(&self, _id: &SessionId, _expected: Option<&str>) -> anyhow::Result<()> {
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

    async fn test_window(cx: &mut TestAppContext) -> (Entity<Workspace>, VisualTestContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/repo", serde_json::json!({ "README.md": "test" }))
            .await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let project = project::Project::test(fs, ["/repo".as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        (workspace, cx.clone())
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
    fn test_ade_connection_claims_ssh_and_local_but_not_wsl_or_docker() {
        let ssh_options = RemoteConnectionOptions::Ssh(ssh("wsl-box", Some("kingii"), None));
        assert_eq!(
            ade_connection(Some(ssh_options)),
            Some(Some(ssh("wsl-box", Some("kingii"), None)))
        );

        // No remote options is an ordinary local project. Local ADE workspaces
        // need a unix daemon (see `WorkspaceLifecycleService::new`), so the
        // expected verdict is spelled the same way here.
        assert_eq!(
            ade_connection(None),
            if cfg!(unix) { Some(None) } else { None }
        );

        // WSL and Docker keep the plain terminal: ADE's session layer only
        // speaks ssh or local, never these transports.
        assert_eq!(
            ade_connection(Some(RemoteConnectionOptions::Wsl(WslConnectionOptions {
                distro_name: "Ubuntu".to_owned(),
                user: None,
            }))),
            None
        );
        assert_eq!(
            ade_connection(Some(RemoteConnectionOptions::Docker(
                DockerConnectionOptions::default()
            ))),
            None
        );
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

    /// Rows as a reconciled pass hands them over, for the matching tests.
    fn rows(workspaces: Vec<AdeWorkspace>) -> Vec<WorkspaceEntry> {
        workspaces
            .into_iter()
            .map(|workspace| WorkspaceEntry::Persisted(workspace, SessionState::Unknown))
            .collect()
    }

    #[test]
    fn test_most_recent_workspace_requires_the_exact_root() {
        let opened_at = |seconds| time::OffsetDateTime::from_unix_timestamp(seconds).unwrap();
        let ssh = ssh("wsl-box", Some("kingii"), None);
        let mut exact = AdeWorkspace::new("main", "testproj", "/home/kingii/testproj");
        exact.remote_host = Some("wsl-box".to_owned());
        exact.daemon_id = Some("daemon-a".to_owned());
        exact.last_opened_at = opened_at(100);
        let mut inside_new = AdeWorkspace::new("new", "testproj", "/home/kingii/testproj/wt");
        inside_new.remote_host = Some("kingii@wsl-box".to_owned());
        inside_new.daemon_id = Some("daemon-a".to_owned());
        inside_new.last_opened_at = opened_at(200);
        let mut elsewhere = AdeWorkspace::new("elsewhere", "other", "/home/kingii/other");
        elsewhere.remote_host = Some("wsl-box".to_owned());
        elsewhere.daemon_id = Some("daemon-a".to_owned());
        elsewhere.last_opened_at = opened_at(300);
        let mut other_host = AdeWorkspace::new("other-host", "testproj", "/home/kingii/testproj");
        other_host.remote_host = Some("gpu-box".to_owned());
        other_host.daemon_id = Some("daemon-b".to_owned());
        other_host.last_opened_at = opened_at(400);

        let workspaces = rows(vec![exact, inside_new, elsewhere, other_host]);
        let best =
            most_recent_workspace(Some(&ssh), Path::new("/home/kingii/testproj"), &workspaces);
        assert_eq!(best.map(|workspace| workspace.name.as_str()), Some("main"));

        let best = most_recent_workspace(
            Some(&ssh),
            Path::new("/home/kingii/testproj/wt"),
            &workspaces,
        );
        assert_eq!(best.map(|workspace| workspace.name.as_str()), Some("new"));
    }

    /// Local matching (no ssh connection) selects only a workspace with no
    /// `remote_host` of its own, never a row a remote host created — even one
    /// pointed at the identical path, which a stale or migrated remote row
    /// could be.
    #[test]
    fn test_local_matching_ignores_remote_workspaces() {
        let mut remote = AdeWorkspace::new("remote", "testproj", "/home/kingii/testproj");
        remote.remote_host = Some("wsl-box".to_owned());
        let mut local = AdeWorkspace::new("local", "testproj", "/home/kingii/testproj");
        local.remote_host = None;

        let workspaces = rows(vec![remote.clone()]);
        assert_eq!(
            most_recent_workspace(None, Path::new("/home/kingii/testproj"), &workspaces),
            None
        );

        let workspaces = rows(vec![remote, local]);
        let best = most_recent_workspace(None, Path::new("/home/kingii/testproj"), &workspaces);
        assert_eq!(best.map(|workspace| workspace.name.as_str()), Some("local"));
    }

    #[test]
    fn test_identityless_rows_require_the_exact_route() {
        let ssh = ssh("wsl-box", Some("kingii"), None);
        let mut workspace = AdeWorkspace::new("main", "testproj", "/home/user/testproj");
        workspace.remote_host = Some("wsl-box".to_owned());
        assert!(!persisted_workspace_matches_connection(
            Some(&ssh),
            &workspace
        ));

        workspace.daemon_id = Some("daemon-a".to_owned());
        assert!(persisted_workspace_matches_connection(
            Some(&ssh),
            &workspace
        ));
    }

    /// A discovery for this host and exact root blocks duplicate creation.
    #[test]
    fn test_a_discovery_for_the_connected_root_blocks_creation() {
        let ssh = ssh("wsl-box", Some("kingii"), None);
        let discovered =
            |id: &str, root: &str, host: &str, created_at| WorkspaceEntry::Discovered {
                remote_host: Some(host.to_owned()),
                workspace: crate::BackendWorkspace {
                    id: id.to_owned(),
                    name: id.to_owned(),
                    project_id: None,
                    project_identity: None,
                    project_root: root.to_owned(),
                    project_scope_rev: 0,
                    created_at,
                },
                state: SessionState::Unknown,
            };
        let entries = vec![
            discovered("ade-b", "/home/user/testproj", "wsl-box", 200),
            discovered("ade-a", "/home/user/testproj", "kingii@wsl-box", 100),
            discovered("ade-elsewhere", "/home/user/other", "wsl-box", 50),
            discovered("ade-other-host", "/home/user/testproj", "gpu-box", 10),
        ];

        assert!(has_discovered_workspace(
            Some(&ssh),
            Path::new("/home/user/testproj"),
            &entries
        ));

        assert!(
            !has_discovered_workspace(None, Path::new("/home/user/testproj"), &entries),
            "a local connection must not treat a remote host's record as its own"
        );
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

    #[cfg(unix)]
    #[gpui::test]
    async fn test_a_fake_local_project_is_not_claimed(cx: &mut TestAppContext) {
        let (workspace, mut window) = test_window(cx).await;
        let claimed = window.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                open_connection_workspace(workspace, window, cx)
            })
        });

        assert!(!claimed);
    }

    /// The short deadline is a guess about a root that has not arrived yet, and
    /// a project whose worktree lands a second late must not be answered with a
    /// stock terminal for the life of the window.
    #[gpui::test]
    async fn test_a_window_the_flow_gave_up_on_can_be_claimed_again(cx: &mut TestAppContext) {
        let (workspace, mut window) = test_window(cx).await;
        let id = workspace.entity_id();
        let this = workspace.downgrade();
        window.update(|_, cx| {
            assert!(claim_window(id, cx), "the first caller takes the window");
            assert!(!claim_window(id, cx), "and the flow runs once per window");
        });

        window
            .update(|window, cx| window.spawn(cx, async move |cx| give_up_on_window(id, &this, cx)))
            .await;

        window.update(|_, cx| {
            assert!(
                claim_window(id, cx),
                "a root that lands late must still find the window claimable"
            );
        });
    }

    #[gpui::test]
    async fn test_project_scope_ignores_a_stale_project_group_hint(cx: &mut TestAppContext) {
        let (workspace, mut window) = test_window(cx).await;
        window.update(|_, cx| {
            workspace.update(cx, |workspace, _cx| {
                workspace.test_set_project_group_key_hint(project::ProjectGroupKey::new(
                    None,
                    util::path_list::PathList::new(&[Path::new("/stale-worktree")]),
                ));
            });
        });

        let workspace = workspace.downgrade();
        let scope = window
            .update(|window, cx| {
                window.spawn(cx, async move |cx| {
                    wait_for_project_root(&workspace, std::time::Duration::ZERO, cx).await
                })
            })
            .await
            .expect("the scanned project has a canonical scope");

        assert_eq!(scope.repository_path, Path::new("/repo"));
        assert_eq!(scope.project_id, "repo");
        assert_eq!(scope.project_identity, "/repo");
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
