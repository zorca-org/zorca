//! A fresh window — local or a fresh ssh connection — reattaches to a daemon
//! workspace.
//!
//! The first window on a fresh connection used to open a plain centre-pane
//! shell that died with the window, so connecting twice (or just closing and
//! reopening Zorca) left throwaway shells and nothing to come back to. This
//! module is the other branch of that fresh-window decision: it asks the right
//! daemon — this machine's or the ssh host's — what it already holds, and then
//! reattaches to the project's most recently opened workspace, adopts one only
//! the daemon knows, or creates the first.
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
    attach::{attach_terminal, open_probed_workspace_session},
    open_workspace_session,
    store::AdeWorkspaceStore,
    workspace_view::{ensure_repository_worktree, name_window_after_workspace},
};
use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, EntityId, Global, PromptLevel, WeakEntity,
    Window,
};
use remote::{RemoteConnectionOptions, SshConnectionOptions};
use std::collections::{HashMap, hash_map::Entry};
use std::path::Path;
use std::sync::Arc;
use util::ResultExt as _;
use workspace::Workspace;

// Completed claims still deduplicate connection flows, but must not block
// agent launches in windows that fell back to ordinary terminals.
enum ClaimState {
    InFlight(Arc<()>),
    Completed,
}

#[derive(Default)]
struct ClaimedWindows(HashMap<EntityId, ClaimState>);

impl Global for ClaimedWindows {}

/// A token when this window is now the caller's to run the flow in.
fn claim_window(window: EntityId, cx: &mut App) -> Option<Arc<()>> {
    match cx.default_global::<ClaimedWindows>().0.entry(window) {
        Entry::Vacant(entry) => {
            let claim = Arc::new(());
            entry.insert(ClaimState::InFlight(claim.clone()));
            Some(claim)
        }
        Entry::Occupied(_) => None,
    }
}

pub(crate) fn connection_is_in_flight(window: EntityId, cx: &App) -> bool {
    cx.try_global::<ClaimedWindows>()
        .and_then(|claims| claims.0.get(&window))
        .is_some_and(|state| matches!(state, ClaimState::InFlight(_)))
}

fn complete_window_claim(window: EntityId, claim: &Arc<()>, cx: &mut App) {
    if cx.has_global::<ClaimedWindows>()
        && let Some(state) = cx.global_mut::<ClaimedWindows>().0.get_mut(&window)
        && matches!(state, ClaimState::InFlight(current) if Arc::ptr_eq(current, claim))
    {
        // A released claim may already have been replaced by a retry.
        *state = ClaimState::Completed;
    }
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
    let Some(claim) = claim_window(window_id, cx) else {
        return true;
    };
    // The one line that proves the flow started; everything after it either
    // attaches or explains itself in this same log.
    log::info!("ADE claims the {label} window; reattaching once the project root settles");

    // A window restored at startup fires its workspace-added hook before the
    // project's worktrees have loaded, so the project root cannot be read here
    // — it is awaited inside the flow. How patient to be is decided by what
    // the window already shows: a restored layout gives the user something to
    // look at while a slow host connects; an empty window owes them a shell
    // fast, and a connection opened with no folder at all never grows a
    // worktree, which the short deadline turns into the plain terminal. The
    // deadline covers only the worktree's absence: its identity comes from
    // the daemon when a previous connect stored one, and otherwise from the
    // scan, which for a large remote tree lands seconds after the root.
    let restored = workspace
        .panes()
        .iter()
        .any(|pane| pane.read(cx).items_len() > 0);
    let root_deadline = if restored {
        IDENTITY_DEADLINE
    } else {
        std::time::Duration::from_secs(3)
    };

    let lifecycle = crate::lifecycle_service(cx);
    cx.spawn_in(window, async move |this, cx| {
        let Some(repository_path) = wait_for_worktree_root(&this, root_deadline, cx).await else {
            log::info!(
                "ADE waited {root_deadline:?} for {label}'s project root; \
                 releasing the claim so a later caller can try again"
            );
            give_up_on_window(window_id, &this, cx);
            return;
        };
        // Connecting with nothing but `~` filled in is how the remote picker
        // behaves before a folder is chosen; a workspace rooted at the whole
        // account is not a project (operator ruling, 2026-08-05). The same
        // ruling applies to a local window rooted at the user's own home.
        let is_home = match &ssh {
            Some(ssh) => is_remote_home_directory(&repository_path, ssh),
            None => repository_path == *util::paths::home_dir(),
        };
        if is_home {
            cx.update(|_, cx| complete_window_claim(window_id, &claim, cx))
                .log_err();
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
            Err(error)
                if host.is_some() && crate::daemon_backend::is_incompatible_daemon(&error) =>
            {
                log::warn!("ADE found an incompatible session daemon on {label}: {error:#}");
                match offer_incompatible_daemon_upgrade(&label, lifecycle.clone(), cx).await {
                    Ok(true) => {
                        cx.background_spawn({
                            let lifecycle = lifecycle.clone();
                            let host = host.clone();
                            async move { lifecycle.ensure_host_workspaces(host.as_deref()).await }
                        })
                        .await
                    }
                    Ok(false) => {
                        cx.update(|_, cx| complete_window_claim(window_id, &claim, cx))
                            .log_err();
                        return;
                    }
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
                        cx.update(|_, cx| complete_window_claim(window_id, &claim, cx))
                            .log_err();
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
                cx.update(|_, cx| complete_window_claim(window_id, &claim, cx))
                    .log_err();
                open_plain_terminal_if_empty(&this, cx);
                return;
            }
        };

        // The identity a previous connection wrote for this root is good
        // enough to open on; the scan that recomputes it can take a large
        // remote tree many seconds, and a known project must not wait for it.
        // The scan's answer still lands, after the open, below.
        let at_root = entries_at_root(ssh.as_ref(), &workspaces, &repository_path)
            .cloned()
            .collect::<Vec<_>>();
        let known_scope = known_project_scope(&at_root);
        let (project_id, project_identity) = match known_scope.clone() {
            Some(scope) => scope,
            None => match wait_for_project_identity(&this, cx).await {
                Some(scope) => scope,
                None => {
                    log::info!(
                        "ADE waited {IDENTITY_DEADLINE:?} for {label}'s project identity; \
                         releasing the claim so a later caller can try again"
                    );
                    give_up_on_window(window_id, &this, cx);
                    return;
                }
            },
        };

        // Give every candidate the connection's canonical project identity
        // before the resolver re-lists and chooses one under the daemon lock.
        let scoped = cx
            .background_spawn({
                let lifecycle = lifecycle.clone();
                let host = host.clone();
                let at_root = at_root.clone();
                let scope = (project_id.clone(), project_identity.clone());
                async move { scope_entries(&lifecycle, host.as_deref(), &at_root, &scope).await }
            })
            .await;
        if let Err(error) = scoped {
            log::warn!("updating ADE project identity for {label} failed: {error:#}");
            cx.update(|_, cx| complete_window_claim(window_id, &claim, cx))
                .log_err();
            open_plain_terminal_if_empty(&this, cx);
            return;
        }

        cx.update(|_, cx| {
            if let Some(store) = AdeWorkspaceStore::try_global(cx) {
                store.update(cx, |store, cx| store.refresh(cx));
            }
        })
        .ok();

        let resolved = cx
            .background_spawn({
                let lifecycle = lifecycle.clone();
                let host = host.clone();
                let root = repository_path.clone();
                let project_scope = Some((project_id, project_identity));
                async move {
                    lifecycle
                        .adopt_or_create_workspace(
                            root,
                            host,
                            project_scope,
                        )
                        .await
                }
            })
            .await;
        let resolved_id = resolved.as_ref().ok().map(|(workspace, _)| workspace.id.clone());
        let opened = match resolved {
            Ok((created, true)) => open_in_window(&this, created.id, cx).await,
            Ok((existing, false)) => open_or_recreate(&this, &lifecycle, existing, cx).await,
            Err(error) => Err(error),
        };

        let opened_id = match opened {
            Ok(_) => resolved_id,
            Err(error) => {
                // The window must not come up empty: whatever the workspace
                // path could not deliver, the connection still owes the user
                // a shell.
                log::warn!("opening the ADE workspace for {label} failed: {error:#}");
                open_plain_terminal_if_empty(&this, cx);
                None
            }
        };
        cx.update(|_, cx| complete_window_claim(window_id, &claim, cx))
            .log_err();

        // A repo that moved since the daemon last saw it: the scan is the
        // authority, the daemon's copy was only a head start. Every entry
        // at the root was stamped with that copy, so every one is corrected;
        // a discovery confirmed into the opened row is reached by its id.
        let (Some(id), Some(known)) = (opened_id, known_scope) else {
            return;
        };
        let Some(scanned) = wait_for_project_identity(&this, cx).await else {
            return;
        };
        if scanned == known {
            return;
        }
        log::info!("ADE corrects {label}'s stored project identity from the scan");
        let corrected = cx
            .background_spawn(async move {
                scope_entries(&lifecycle, host.as_deref(), &at_root, &scanned).await?;
                lifecycle
                    .update_workspace_project_scope(&id, &scanned.0, &scanned.1)
                    .await?;
                anyhow::Ok(())
            })
            .await;
        if let Err(error) = corrected {
            log::warn!("correcting ADE project identity for {label} failed: {error:#}");
            return;
        }
        cx.update(|_, cx| {
            if let Some(store) = AdeWorkspaceStore::try_global(cx) {
                store.update(cx, |store, cx| store.refresh(cx));
            }
        })
        .ok();
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

/// How long a worktree may take to report its identity once it exists. A
/// remote worktree sends its git root and completed scan only with the last
/// scan update, which for a large tree is many seconds behind the first entry.
const IDENTITY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

const ROOT_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// Polls `read` every [`ROOT_POLL`] until it answers or `deadline` passes.
async fn poll_until<T>(
    this: &WeakEntity<Workspace>,
    deadline: std::time::Duration,
    cx: &mut AsyncWindowContext,
    mut read: impl FnMut(&mut Workspace, &mut Context<Workspace>) -> Option<T>,
) -> Option<T> {
    let mut waited = std::time::Duration::ZERO;
    loop {
        if let Some(value) = this.update(cx, |workspace, cx| read(workspace, cx)).ok()? {
            return Some(value);
        }
        if waited >= deadline {
            return None;
        }
        cx.background_executor().timer(ROOT_POLL).await;
        waited += ROOT_POLL;
    }
}

/// The first visible worktree's root, awaited because a restored window loads
/// its worktrees after the workspace exists. `None` means the deadline passed
/// with the project still rootless.
async fn wait_for_worktree_root(
    this: &WeakEntity<Workspace>,
    deadline: std::time::Duration,
    cx: &mut AsyncWindowContext,
) -> Option<std::path::PathBuf> {
    poll_until(this, deadline, cx, |workspace, cx| {
        workspace
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    })
    .await
}

/// The project's `(project_id, project_identity)` as the scan derives it —
/// `None` past [`IDENTITY_DEADLINE`].
async fn wait_for_project_identity(
    this: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> Option<(String, String)> {
    poll_until(this, IDENTITY_DEADLINE, cx, |workspace, cx| {
        if !workspace.project_group_identity_is_known(cx) {
            return None;
        }
        let project_group_key = workspace.project().read(cx).project_group_key(cx);
        let project_identity = project_group_key.path_list().serialize().paths;
        if project_identity.is_empty() {
            return None;
        }
        Some((
            project_group_key
                .display_name(&Default::default())
                .to_string(),
            project_identity,
        ))
    })
    .await
}

/// The listing's entries rooted at `root` on this connection.
fn entries_at_root<'a>(
    ssh: Option<&'a SshConnectionOptions>,
    workspaces: &'a [WorkspaceEntry],
    root: &'a Path,
) -> impl Iterator<Item = &'a WorkspaceEntry> + 'a {
    workspaces.iter().filter(move |entry| match entry {
        WorkspaceEntry::Persisted(workspace, _) => {
            persisted_workspace_matches_connection(ssh, workspace)
                && workspace.repository_path == root
        }
        WorkspaceEntry::Discovered {
            remote_host,
            workspace,
            ..
        } => {
            workspace_matches_connection(ssh, remote_host.as_deref())
                && Path::new(&workspace.project_root) == root
        }
    })
}

/// The `(project_id, project_identity)` the entries at a root already hold —
/// written by an earlier connect's scan. A persisted row beats a discovery,
/// and an entry with no identity yet contributes nothing.
fn known_project_scope(at_root: &[WorkspaceEntry]) -> Option<(String, String)> {
    let persisted = at_root
        .iter()
        .filter_map(WorkspaceEntry::persisted)
        .filter_map(|(workspace, _)| {
            Some((
                workspace.project_id.clone(),
                workspace.project_identity.clone()?,
            ))
        });
    let discovered = at_root.iter().filter_map(|entry| match entry {
        WorkspaceEntry::Discovered { workspace, .. } => Some((
            workspace.project_id.clone()?,
            workspace.project_identity.clone()?,
        )),
        _ => None,
    });
    persisted
        .chain(discovered)
        .find(|(id, identity)| !id.is_empty() && !identity.is_empty())
}

/// Writes `scope` to every entry at a root, so the resolver and the sidebar
/// see one identity for the project. Unchanged entries are cheap: the
/// lifecycle skips them.
async fn scope_entries(
    lifecycle: &WorkspaceLifecycleService,
    host: Option<&str>,
    at_root: &[WorkspaceEntry],
    (project_id, project_identity): &(String, String),
) -> anyhow::Result<()> {
    for entry in at_root {
        match entry {
            WorkspaceEntry::Persisted(workspace, _) => {
                lifecycle
                    .update_workspace_project_scope(&workspace.id, project_id, project_identity)
                    .await?;
            }
            WorkspaceEntry::Discovered { workspace, .. } => {
                lifecycle
                    .update_discovered_workspace_project_scope(
                        host,
                        &workspace.id,
                        project_id,
                        project_identity,
                    )
                    .await?;
            }
        }
    }
    Ok(())
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
    if !matches!(probed.1, SessionState::Dead) {
        return this
            .update_in(cx, |_, window, cx| {
                open_probed_workspace_session(&cx.entity(), probed, window, cx)
            })?
            .await;
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
        // The startup fallback keeps its window empty on purpose so the
        // launchpad shows; dropping a shell in would hide it. The suppression
        // lifts once the window gains a project root, so later connect flows
        // get their fallback shell as usual.
        if workspace.fresh_window_item_suppressed() {
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
        session_exists: bool,
        probes: AtomicUsize,
        layout_reads: AtomicUsize,
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

        fn exists(&self, _id: &SessionId, expected: Option<&str>) -> anyhow::Result<bool> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            if self.session_exists {
                assert_eq!(expected, Some("test-daemon"));
            }
            Ok(self.session_exists)
        }

        fn open_workspace(
            &self,
            _workspace_id: &str,
            expected: Option<&str>,
        ) -> anyhow::Result<crate::WorkspaceLayout> {
            assert_eq!(expected, Some("test-daemon"));
            self.layout_reads.fetch_add(1, Ordering::SeqCst);
            Ok(crate::WorkspaceLayout {
                layout: ade_session::LayoutDoc::empty(),
                rev: 1,
            })
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

    #[gpui::test]
    async fn test_reconnect_probes_a_live_workspace_once(cx: &mut TestAppContext) {
        let (workspace, mut window) = test_window(cx).await;
        let backend = Arc::new(UpgradeBackend {
            session_exists: true,
            ..Default::default()
        });
        let registry = crate::AdeWorkspaceRegistry::open_test_db("reconnect_probes_once").await;
        let mut row = AdeWorkspace::new("main", "repo", "/repo");
        row.terminal_session_id = Some(row.daemon_workspace_id());
        row.daemon_id = Some("test-daemon".to_owned());
        let id = row.id.clone();
        registry.create_workspace(row.clone()).await.unwrap();
        let lifecycle = Arc::new(WorkspaceLifecycleService::with_backend(
            registry,
            backend.clone(),
        ));
        window.update(|_, cx| cx.set_global(crate::GlobalLifecycleService(lifecycle.clone())));
        let open = window.update(|window, cx| {
            window.spawn(cx, {
                let workspace = workspace.downgrade();
                async move |cx| open_or_recreate(&workspace, &lifecycle, row, cx).await
            })
        });
        open.await.unwrap();
        assert_eq!(backend.probes.load(Ordering::SeqCst), 1);
        assert_eq!(backend.layout_reads.load(Ordering::SeqCst), 1);
        assert!(workspace.read_with(&window, |workspace, _| workspace.ade_owns_layout()));

        let reopen = window.update(|window, cx| open_workspace_session(&workspace, id, window, cx));
        reopen.await.unwrap();
        assert_eq!(backend.probes.load(Ordering::SeqCst), 2);
        assert_eq!(backend.layout_reads.load(Ordering::SeqCst), 2);
    }

    #[gpui::test]
    async fn test_releasing_a_claim_allows_the_window_to_reconnect(cx: &mut TestAppContext) {
        let (workspace, mut window) = test_window(cx).await;
        window.update(|_, cx| {
            let id = workspace.entity_id();
            assert!(claim_window(id, cx).is_some());
            release_window_claim(id, cx);
            assert!(claim_window(id, cx).is_some());
        });
    }

    #[gpui::test]
    async fn test_old_completion_does_not_finish_a_reclaimed_window(cx: &mut TestAppContext) {
        let (workspace, mut window) = test_window(cx).await;
        window.update(|_, cx| {
            let id = workspace.entity_id();
            let old = claim_window(id, cx).unwrap();
            release_window_claim(id, cx);
            let current = claim_window(id, cx).unwrap();
            complete_window_claim(id, &old, cx);
            assert!(connection_is_in_flight(id, cx));
            complete_window_claim(id, &current, cx);
            assert!(!connection_is_in_flight(id, cx));
            assert!(claim_window(id, cx).is_none());
        });
    }

    #[gpui::test]
    async fn test_an_in_flight_connection_blocks_agent_preset(cx: &mut TestAppContext) {
        let (workspace, mut window) = test_window(cx).await;
        window.update(|_, cx| {
            let id = workspace.entity_id();
            assert!(claim_window(id, cx).is_some());
            assert!(
                claim_window(id, cx).is_none(),
                "duplicate callers share the flow"
            );
        });
        let blocked = window.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                crate::layout::open_agent_terminal(workspace, window, cx).is_some()
            })
        });
        assert!(
            blocked,
            "an in-flight connection must reject a throwaway PTY"
        );
    }

    #[gpui::test]
    async fn test_completed_home_or_failure_fallback_does_not_block_agent_preset(
        cx: &mut TestAppContext,
    ) {
        let (workspace, mut window) = test_window(cx).await;
        window.update(|_, cx| {
            let id = workspace.entity_id();
            let claim = claim_window(id, cx).unwrap();
            complete_window_claim(id, &claim, cx);
            assert!(!connection_is_in_flight(id, cx));
        });
        let allowed = window.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                crate::layout::open_agent_terminal(workspace, window, cx).is_none()
            })
        });
        assert!(allowed, "a completed fallback must allow a throwaway PTY");
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
            assert!(
                claim_window(id, cx).is_some(),
                "the first caller takes the window"
            );
            assert!(
                claim_window(id, cx).is_none(),
                "and the flow runs once per window"
            );
            assert!(connection_is_in_flight(id, cx));
        });

        window
            .update(|window, cx| window.spawn(cx, async move |cx| give_up_on_window(id, &this, cx)))
            .await;

        window.update(|_, cx| {
            assert!(!connection_is_in_flight(id, cx));
            assert!(
                claim_window(id, cx).is_some(),
                "a root that lands late must still find the window claimable"
            );
            assert!(connection_is_in_flight(id, cx));
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
        let (root, scope) = window
            .update(|window, cx| {
                window.spawn(cx, async move |cx| {
                    let root =
                        wait_for_worktree_root(&workspace, std::time::Duration::ZERO, cx).await;
                    (root, wait_for_project_identity(&workspace, cx).await)
                })
            })
            .await;
        let scope = scope.expect("the scanned project has a canonical scope");

        assert_eq!(root.as_deref(), Some(Path::new("/repo")));
        assert_eq!(scope.0, "repo");
        assert_eq!(scope.1, "/repo");
    }

    #[test]
    fn test_a_known_identity_comes_from_the_listing_before_the_scan() {
        let host = Some("kingii@prime-form".to_owned());
        let mut scoped = AdeWorkspace::new("main", "commerce", "/home/kingii/commerce");
        scoped.remote_host = host.clone();
        scoped.project_identity = Some("/home/kingii/commerce".to_owned());
        let mut unscoped = AdeWorkspace::new("main", "commerce", "/home/kingii/commerce");
        unscoped.remote_host = host.clone();
        let mut elsewhere = AdeWorkspace::new("main", "other", "/home/kingii/other");
        elsewhere.remote_host = host.clone();
        elsewhere.project_identity = Some("/home/kingii/other".to_owned());
        let discovered =
            |project_id: Option<&str>, identity: Option<&str>| WorkspaceEntry::Discovered {
                remote_host: host.clone(),
                workspace: crate::session_backend::BackendWorkspace {
                    id: "w1".to_owned(),
                    name: "main".to_owned(),
                    project_id: project_id.map(str::to_owned),
                    project_identity: identity.map(str::to_owned),
                    project_root: "/home/kingii/commerce".to_owned(),
                    project_scope_rev: 0,
                    created_at: 0,
                },
                state: SessionState::Unknown,
            };
        let prime_form = ssh("prime-form", Some("kingii"), None);
        let root = Path::new("/home/kingii/commerce");
        let persisted =
            |row: &AdeWorkspace| WorkspaceEntry::Persisted(row.clone(), SessionState::Unknown);

        let known = |ssh: &SshConnectionOptions, entries: &[WorkspaceEntry]| {
            let at_root = entries_at_root(Some(ssh), entries, root)
                .cloned()
                .collect::<Vec<_>>();
            known_project_scope(&at_root)
        };

        // Nothing at this root carries an identity: wait for the scan.
        assert_eq!(
            known(
                &prime_form,
                &[
                    persisted(&unscoped),
                    persisted(&elsewhere),
                    discovered(Some("commerce"), None)
                ],
            ),
            None
        );
        // A discovery with both halves is enough.
        assert_eq!(
            known(
                &prime_form,
                &[discovered(Some("commerce"), Some("/home/kingii/commerce"))],
            ),
            Some(("commerce".to_owned(), "/home/kingii/commerce".to_owned()))
        );
        // A row on another connection is not this project.
        assert_eq!(
            known(&ssh("elsewhere", None, None), &[persisted(&scoped)]),
            None
        );
        // A persisted row wins over a discovery.
        assert_eq!(
            known(
                &prime_form,
                &[
                    discovered(Some("stale"), Some("/stale")),
                    persisted(&scoped)
                ],
            ),
            Some(("commerce".to_owned(), "/home/kingii/commerce".to_owned()))
        );
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
