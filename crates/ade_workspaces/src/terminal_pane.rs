//! The persistent terminal pane: a center editor item running a real client
//! attached to a workspace's session — `ade-daemon attach` since 2026-08-03,
//! `tmux attach` before it.
//!
//! **A dumb pane.** This module opens (or refocuses) a terminal on the argv it
//! is handed and nothing else. It does not probe whether the session is alive,
//! does not recreate a dead one, and does not report status — that belongs to
//! the sidebar, over [`crate::WorkspaceLifecycleService`]. The argv from
//! [`crate::WorkspaceLifecycleService::attach_command`] is attach-or-create, so
//! opening a pane is idempotent either way.
//!
//! **The session backend owns the session.** The processes, the scrollback that
//! survives a closed window, and the life of the pty all live behind that argv;
//! Zed contributes a renderer and a tab. Nothing here draws chrome inside the
//! terminal rect.
//!
//! One tab per workspace *per window* is what [`OpenTerminals`] keeps, and it
//! is about the workspace's attach-or-create session only — the extra terminals
//! a window holds are the layout's, one `Tab::Terminal` each, and are not in
//! that map. Attaching twice to one session is legal — the daemon and tmux both
//! mirror their clients — so a stale entry costs the user a duplicate tab,
//! never correctness.

use crate::{
    AdeWorkspace, AdeWorkspaceStore, Attached, WorkspaceId,
    layout::{session_of_item, session_task_id},
};
use anyhow::{Context as _, Result};
use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Entity, Focusable, Global, Task, WeakEntity,
    Window,
};
use std::{collections::HashMap, path::PathBuf, rc::Rc};
use task::SpawnInTerminal;
use terminal_view::TerminalView;
use workspace::{Pane, Workspace};

/// Opens the workspace's terminal as a center item, or activates the one
/// already open for it.
///
/// `attached`'s argv is run verbatim, with the workspace's repository path as
/// the working directory — for a *local* workspace; see [`attach_spawn_task`] —
/// and the workspace's (human, sans-serif) name as the tab title. Callers get
/// it from [`crate::WorkspaceLifecycleService::attach_command`].
///
/// **The attach client always runs on this machine**, which is why the spawn
/// goes through [`project::Project::create_local_terminal_task`] rather than
/// the plain task entry point. That argv is the client end of the daemon
/// transport — it dials a client-side ssh forward or a local socket — so a
/// window whose project is a built-in SSH remote must not route it through the
/// project's remote client: doing so ships it to the remote host, where it
/// dissolves into a login shell and the daemon is never contacted.
pub fn open_workspace_terminal(
    zed_workspace: &mut Workspace,
    ade_workspace: &AdeWorkspace,
    attached: Attached,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<Entity<TerminalView>>> {
    let session = attached.session_id.as_str();
    // Pruning happens on every access, so a workspace whose tab was closed and
    // never reopened does not keep a dead weak handle forever.
    let already_open = cx
        .default_global::<OpenTerminals>()
        .get(&ade_workspace.id, |_| ());
    if let Some(terminal_view) = already_open {
        // **Only if it is showing the session we just attached to.** A recreate
        // makes a *new* session for the same workspace, and refocusing the tab
        // holding the dead one would report success while showing a corpse. The
        // stale tab is left where it is rather than closed: closing kills, and
        // a workspace's other tabs are its live siblings.
        let same_session = session_of_item(&terminal_view, cx).as_deref() == Some(session);
        // Don't steal focus from an open modal — same care as
        // `TerminalPanel::add_center_terminal`.
        let focus_item = !zed_workspace.has_active_modal(window, cx);
        if same_session && zed_workspace.activate_item(&terminal_view, true, focus_item, window, cx)
        {
            return Task::ready(Ok(terminal_view));
        }
        // Either it is another session's, or it is alive but no pane in *this*
        // window holds it — it belongs to another window, or was pulled out
        // from under us. Forget it and attach again; the daemon and tmux both
        // mirror a second client.
        cx.default_global::<OpenTerminals>()
            .forget(&ade_workspace.id);
    }

    if !zed_workspace.project().read(cx).supports_terminal(cx) {
        return Task::ready(Err(anyhow::anyhow!(
            "this project cannot host a terminal, so workspace {} cannot be attached",
            ade_workspace.id
        )));
    }

    let session_id = attached.session_id.clone();
    let spawn_task = match attach_spawn_task(ade_workspace, attached) {
        Ok(spawn_task) => spawn_task,
        Err(error) => return Task::ready(Err(error)),
    };
    let id = ade_workspace.id.clone();
    let title = ade_workspace.name.clone();
    let ade_workspace = ade_workspace.clone();
    let project = zed_workspace.project().downgrade();

    cx.spawn_in(window, async move |zed_workspace, cx| {
        let terminal = project
            .update(cx, |project, cx| {
                project.create_local_terminal_task(spawn_task, cx)
            })?
            .await?;

        zed_workspace.update_in(cx, |zed_workspace, window, cx| {
            let terminal_view = cx.new(|cx| {
                TerminalView::new(
                    terminal,
                    zed_workspace.weak_handle(),
                    zed_workspace.database_id(),
                    zed_workspace.project().downgrade(),
                    window,
                    cx,
                )
            });
            // The tab reads as the workspace, not as the tmux session string —
            // until the session names itself; see `follow_session_title`.
            terminal_view.update(cx, |terminal_view, cx| {
                terminal_view.set_custom_title(Some(title), cx);
            });
            AdeWorkspaceStore::global(cx).update(cx, |store, cx| {
                store.follow_session_title(id.clone(), &terminal_view, cx);
            });
            request_active_resize(&terminal_view, &session_id, &ade_workspace, window, cx);

            let focus_item = !zed_workspace.has_active_modal(window, cx);
            zed_workspace.add_item_to_active_pane(
                Box::new(terminal_view.clone()),
                None,
                focus_item,
                window,
                cx,
            );
            cx.default_global::<OpenTerminals>()
                .insert(id, &terminal_view);
            terminal_view
        })
    })
}

/// The spawn description for one attach: the argv verbatim, rooted at the
/// workspace's checkout, labelled with the workspace's name.
///
/// **A remote workspace gets no working directory.** Its `repository_path` is a
/// path on *its* host, and handing that to a local spawn would either fail or —
/// worse — start the client in whatever unrelated local directory happens to
/// share the path. The argv is unaffected: the attach client is local either
/// way, and the session's own cwd is the host's, set when it was created. A
/// remote workspace therefore composes exactly with the forced-local spawn in
/// [`open_workspace_terminal`]: no cwd to resolve, and no remote client to
/// resolve it against.
///
/// **Identified by the session, not by the workspace.** It used to be
/// `ade-workspace:<registry id>`, which [`crate::layout`] does not recognise —
/// so a window opened this way captured a document without its own terminal in
/// it, and the first push deleted the tab for everybody. The attach now says
/// which session it landed on ([`Attached`]), so the tab reads back as the
/// `Tab::Terminal` it is.
fn attach_spawn_task(ade_workspace: &AdeWorkspace, attached: Attached) -> Result<SpawnInTerminal> {
    session_spawn_task(
        &attached.session_id,
        &ade_workspace.name,
        (!ade_workspace.is_remote()).then(|| ade_workspace.repository_path.clone()),
        attached.argv,
    )
}

/// Attaches a terminal to one daemon session as an item of `pane` — the
/// terminal half of rendering a [`crate::layout`] document.
///
/// Three things separate this from [`open_workspace_terminal`], and all three
/// come from the layout being the daemon's:
///
/// - **It never creates.** `attach_argv` names a session the daemon already
///   has ([`crate::WorkspaceLifecycleService::attach_session_command`]), so a
///   tab whose session died stays dead rather than quietly becoming a shell.
/// - **It lands in the pane it is given**, not the active one: the pane tree is
///   built before anything is focused, and "active" is meaningless mid-build.
/// - **The session's id is on the tab.** The spawn task is
///   [`session_task_id`], which is how [`crate::layout`] reads a terminal back
///   as a `Tab::Terminal` however far the user drags it.
///
/// The spawn is still forced local — see [`open_workspace_terminal`] for why
/// the attach client must not be shipped to a remote project's host.
pub(crate) async fn open_session_terminal(
    zed_workspace: &WeakEntity<Workspace>,
    pane: &WeakEntity<Pane>,
    session_id: &str,
    ade_workspace: &AdeWorkspace,
    cwd: Option<PathBuf>,
    attach_argv: Vec<String>,
    destination_index: Option<usize>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<TerminalView>> {
    let terminal_view = create_session_terminal(
        zed_workspace,
        session_id,
        ade_workspace,
        cwd,
        attach_argv,
        cx,
    )
    .await?;

    pane.update_in(cx, |pane, window, cx| {
        // Neither activating the pane nor focusing the item: the layout says
        // which pane is focused and which tab is active, and it is applied once
        // the whole tree exists.
        pane.add_item_inner(
            Box::new(terminal_view.clone()),
            false,
            false,
            false,
            destination_index,
            window,
            cx,
        );
    })?;
    Ok(terminal_view)
}

pub(crate) async fn create_session_terminal(
    zed_workspace: &WeakEntity<Workspace>,
    session_id: &str,
    ade_workspace: &AdeWorkspace,
    cwd: Option<PathBuf>,
    attach_argv: Vec<String>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<TerminalView>> {
    let title = ade_workspace.name.as_str();
    let workspace_id = ade_workspace.id.clone();
    let spawn_task = session_spawn_task(session_id, title, cwd, attach_argv)?;
    let terminal = zed_workspace
        .update(cx, |zed_workspace, cx| {
            zed_workspace.project().update(cx, |project, cx| {
                project.create_local_terminal_task(spawn_task, cx)
            })
        })?
        .await?;

    let terminal_view = zed_workspace.update_in(cx, |zed_workspace, window, cx| {
        let terminal_view = cx.new(|cx| {
            TerminalView::new(
                terminal,
                zed_workspace.weak_handle(),
                zed_workspace.database_id(),
                zed_workspace.project().downgrade(),
                window,
                cx,
            )
        });
        terminal_view.update(cx, |terminal_view, cx| {
            terminal_view.set_custom_title(Some(title.to_owned()), cx);
        });
        AdeWorkspaceStore::global(cx).update(cx, |store, cx| {
            store.follow_session_title(workspace_id, &terminal_view, cx);
        });
        request_active_resize(&terminal_view, session_id, ade_workspace, window, cx);
        terminal_view
    })?;

    Ok(terminal_view)
}

fn request_active_resize(
    terminal_view: &Entity<TerminalView>,
    session_id: &str,
    ade_workspace: &AdeWorkspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let focus_handle = terminal_view.focus_handle(cx);
    let weak_terminal_view = terminal_view.downgrade();
    let resize = {
        let ade_workspace = ade_workspace.clone();
        let session_id = session_id.to_owned();
        Rc::new(move |cx: &mut App| {
            let Some(terminal_view) = weak_terminal_view.upgrade() else {
                return;
            };
            let Some((cols, rows)) = terminal_view.read(cx).view_size(cx) else {
                return;
            };
            let lifecycle = crate::lifecycle_service(cx);
            let ade_workspace = ade_workspace.clone();
            let session_id = session_id.clone();
            cx.background_spawn(async move {
                if let Err(error) =
                    lifecycle.resize_session(&ade_workspace, &session_id, cols, rows)
                {
                    log::warn!("session {session_id} could not follow the active size: {error:#}");
                }
            })
            .detach();
        })
    };
    terminal_view.update(cx, |terminal_view, cx| {
        let event_resize = resize.clone();
        terminal_view.set_resize_callback(move |cx| event_resize(cx));
        cx.on_focus(&focus_handle, window, move |_, _, cx| resize(cx))
            .detach();
    });
}

/// The spawn description for one session attach — and, through
/// [`attach_spawn_task`], for every attach there is.
///
/// Deliberately a *task* terminal rather than a shell one. It is the only
/// public [`project::Project`] entry point that runs a chosen argv, and its
/// side effect is the one wanted here: task terminals are excluded from
/// `TerminalView`'s session serialization, so a restarted Zed does not restore
/// this tab as a bare shell divorced from its session. The task chrome is
/// switched off — `show_summary` / `show_command` / `show_rerun` all default to
/// `false` — so the pane is just a terminal.
fn session_spawn_task(
    session_id: &str,
    title: &str,
    cwd: Option<PathBuf>,
    attach_argv: Vec<String>,
) -> Result<SpawnInTerminal> {
    let command_label = attach_argv.join(" ");
    let mut attach_argv = attach_argv.into_iter();
    let command = attach_argv
        .next()
        .context("the attach command is empty; expected a daemon attach argv")?;

    Ok(SpawnInTerminal {
        id: session_task_id(session_id),
        label: title.to_owned(),
        full_label: title.to_owned(),
        command: Some(command),
        args: attach_argv.collect(),
        command_label,
        cwd,
        ..SpawnInTerminal::default()
    })
}

/// Which workspaces currently have a terminal item open, so a second request
/// focuses the existing tab instead of attaching a mirrored client beside it.
///
/// Weak on purpose: the pane, not this map, owns the view's lifetime — closing
/// a tab must drop the terminal, and every read here prunes the handles that
/// closing left behind.
type OpenTerminals = WorkspaceTerminals<TerminalView>;

impl Global for OpenTerminals {}

/// The map behind [`OpenTerminals`], generic only so its pruning can be tested
/// against a trivial entity instead of a real terminal.
struct WorkspaceTerminals<T: 'static> {
    open: HashMap<WorkspaceId, WeakEntity<T>>,
}

impl<T: 'static> Default for WorkspaceTerminals<T> {
    fn default() -> Self {
        Self {
            open: HashMap::new(),
        }
    }
}

impl<T: 'static> WorkspaceTerminals<T> {
    /// The live view for `id`, if there is one, after dropping every handle
    /// whose view is gone.
    ///
    /// `on_pruned` is called with each id that was dropped — the production
    /// caller ignores it; tests assert on it.
    fn get(
        &mut self,
        id: &WorkspaceId,
        mut on_pruned: impl FnMut(&WorkspaceId),
    ) -> Option<Entity<T>> {
        self.open.retain(|id, view| {
            let alive = view.upgrade().is_some();
            if !alive {
                on_pruned(id);
            }
            alive
        });
        self.open.get(id).and_then(WeakEntity::upgrade)
    }

    fn insert(&mut self, id: WorkspaceId, view: &Entity<T>) {
        self.open.insert(id, view.downgrade());
    }

    fn forget(&mut self, id: &WorkspaceId) {
        self.open.remove(id);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.open.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use task::TaskId;

    /// Stands in for a `TerminalView`, which cannot be built without a window.
    /// The map is generic precisely so this substitution is possible.
    struct FakeView;

    #[test]
    fn test_attach_spawn_task_carries_argv_cwd_and_name() {
        let mut workspace = AdeWorkspace::new("Vector DB spike", "project-a", "/repos/zed");
        workspace.id = WorkspaceId::from("0123456789abcdef");

        let spawn_task = attach_spawn_task(
            &workspace,
            Attached {
                session_id: "ade-vector-db-spike-012345".into(),
                daemon_id: None,
                argv: vec![
                    "tmux".into(),
                    "new-session".into(),
                    "-A".into(),
                    "-s".into(),
                    "ade-vector-db-spike-012345".into(),
                    "-c".into(),
                    "/repos/zed".into(),
                ],
            },
        )
        .unwrap();

        // The argv is run verbatim: no shell, no reimplementation of tmux.
        assert_eq!(spawn_task.command.as_deref(), Some("tmux"));
        assert_eq!(
            spawn_task.args,
            vec![
                "new-session",
                "-A",
                "-s",
                "ade-vector-db-spike-012345",
                "-c",
                "/repos/zed"
            ]
        );
        assert_eq!(
            spawn_task.cwd.as_deref(),
            Some(std::path::Path::new("/repos/zed"))
        );
        // The tab reads as the workspace, not as the session string.
        assert_eq!(spawn_task.label, "Vector DB spike");
        assert_eq!(spawn_task.full_label, "Vector DB spike");
        assert_eq!(
            spawn_task.command_label,
            "tmux new-session -A -s ade-vector-db-spike-012345 -c /repos/zed"
        );
        // The *session*, not the workspace: it is what a layout names, so a
        // capture reads this tab back as the terminal it is.
        assert_eq!(
            spawn_task.id,
            TaskId("ade-session:ade-vector-db-spike-012345".into())
        );
        // Task chrome stays off: this is a terminal, not a build.
        assert!(!spawn_task.show_summary);
        assert!(!spawn_task.show_command);
        assert!(!spawn_task.show_rerun);
    }

    /// The remote half of the same task: same argv shape, no local cwd.
    #[test]
    fn test_a_remote_workspace_gets_no_local_working_directory() {
        let mut workspace = AdeWorkspace::new("main", "zed", "/repos/zed");
        workspace.remote_host = Some("build-box".into());

        let spawn_task = attach_spawn_task(
            &workspace,
            Attached {
                session_id: "session-uuid".into(),
                daemon_id: None,
                argv: vec![
                    "/opt/ade/ade-daemon".into(),
                    "attach".into(),
                    "session-uuid".into(),
                    "--socket".into(),
                    "/home/k/.ade/hosts/build-box.sock".into(),
                ],
            },
        )
        .unwrap();

        // The path exists in the argv's *host* sense only, and may even exist
        // locally — which is exactly the case that must not be opened.
        assert_eq!(spawn_task.cwd, None);
        // The client is still a plain local command through a forwarded socket:
        // no ssh in the argv, because that would be a connection per terminal.
        assert_eq!(spawn_task.command.as_deref(), Some("/opt/ade/ade-daemon"));
        assert!(!spawn_task.args.iter().any(|argument| argument == "ssh"));
        assert!(spawn_task.args.contains(&"--socket".to_owned()));
    }

    #[test]
    fn test_empty_attach_argv_is_an_error_not_a_panic() {
        let workspace = AdeWorkspace::new("main", "project-a", "/repos/zed");
        let error = attach_spawn_task(
            &workspace,
            Attached {
                session_id: "s1".into(),
                daemon_id: None,
                argv: Vec::new(),
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("empty"));
    }

    #[test]
    fn test_two_sessions_never_share_a_task_id() {
        let workspace = AdeWorkspace::new("main", "project-a", "/repos/zed");
        let attached = |session_id: &str| Attached {
            session_id: session_id.to_owned(),
            daemon_id: None,
            argv: vec!["tmux".to_owned()],
        };
        assert_ne!(
            attach_spawn_task(&workspace, attached("s1")).unwrap().id,
            attach_spawn_task(&workspace, attached("s2")).unwrap().id
        );
    }

    #[gpui::test]
    async fn test_dead_views_are_pruned_on_access(cx: &mut gpui::TestAppContext) {
        let mut open = WorkspaceTerminals::<FakeView>::default();
        let alive_id = WorkspaceId::new();
        let closed_id = WorkspaceId::new();

        let alive = cx.new(|_| FakeView);
        let closed = cx.new(|_| FakeView);
        open.insert(alive_id.clone(), &alive);
        open.insert(closed_id.clone(), &closed);
        assert_eq!(open.len(), 2);

        // Nothing has been closed yet, so nothing is pruned.
        let mut pruned = Vec::new();
        assert!(
            open.get(&alive_id, |id| pruned.push(id.clone())).is_some(),
            "a live view must be found"
        );
        assert!(pruned.is_empty());

        // The user closes one tab: the pane, not this map, owned the view.
        drop(closed);
        cx.run_until_parked();

        // Asking about the *other* workspace still clears the dead handle, so
        // the map cannot grow without bound.
        let mut pruned = Vec::new();
        assert!(open.get(&alive_id, |id| pruned.push(id.clone())).is_some());
        assert_eq!(pruned, vec![closed_id.clone()]);
        assert_eq!(open.len(), 1);
        assert!(open.get(&closed_id, |_| ()).is_none());

        // Forgetting is idempotent, and reinserting takes the slot back.
        open.forget(&alive_id);
        assert_eq!(open.len(), 0);
        open.forget(&alive_id);
        open.insert(alive_id.clone(), &alive);
        assert_eq!(
            open.get(&alive_id, |_| ()).map(|view| view.entity_id()),
            Some(alive.entity_id())
        );
    }
}
