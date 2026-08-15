//! Opening a workspace from anywhere in the app: mark it opened, ask the
//! backend what state its session is in, and attach unless the session is dead.
//!
//! The one entry point every surface uses — the scaffold panel's rows and the
//! ledger sidebar's — so that "clicking a workspace" means the same thing
//! wherever it is clicked, and the three rules of the lifecycle layer are
//! honoured once instead of per caller:
//!
//! - **Selecting attaches; it never repairs.** A session that probed
//!   [`SessionState::Dead`] is left alone for the caller to surface; only an
//!   explicit recreate brings it back.
//! - **`NeverCreated` attaches too.** The argv is attach-or-create, so the
//!   first open of a workspace that has no session makes it and attaches in one
//!   step — and the registry is then told the name it now points at.
//! - **Every lifecycle call blocks**, so all of them run on the background
//!   executor.
//!
//! And one rule about the window rather than the session: **opening ends Zed's
//! own restore path for it.** The window is marked with
//! [`Workspace::set_ade_owns_layout`], after which Zed serializes an empty
//! centre for it and so has nothing to restore over the daemon's layout. Two
//! persistence systems over one window is how a restore brings back fresh
//! shells; the daemon is the only one that gets to answer.
//!
//! And one about what happens after: **a window holding a daemon terminal
//! syncs**, whether it was built from a stored arrangement or attached without
//! one. The two paths used to differ — an attached window installed no
//! [`crate::LayoutSync`], so a terminal added to it later reached no document
//! and died with the window — and [`attach_terminal`] is where they meet. It is
//! the one entry point for every attach in the app: this module's fallback, the
//! create-workspace modal, and the sidebar's recreate.

use crate::{
    AdeLayouts, AdeWorkspace, Attached, MissingTab, SessionState, WorkspaceId, WorkspaceLayout,
    open_workspace_terminal, render_layout,
    store::AdeWorkspaceStore,
    workspace_view::{
        bound_workspace_for_worktree, ensure_repository_worktree, name_window_after_workspace,
    },
};
use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, AsyncWindowContext, Entity, Task, WeakEntity, Window};
use std::path::{Path, PathBuf};
use util::ResultExt as _;
use workspace::{SaveIntent, Workspace};

/// Opens (or refocuses) the terminal attached to workspace `id` in this window.
///
/// `zed_workspace` must be the window's *active* workspace: the terminal lands
/// in its active pane, and a tab in any other workspace would never be on
/// screen. Refocusing is [`open_workspace_terminal`]'s job — a workspace whose
/// tab is already open is activated rather than attached a second time.
///
/// The returned task refreshes the [`AdeWorkspaceStore`] before it resolves, so
/// every view's dots catch up with whatever the open did.
pub fn open_workspace_session(
    zed_workspace: &Entity<Workspace>,
    id: WorkspaceId,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<()>> {
    let lifecycle = crate::lifecycle_service(cx);
    let zed_workspace = zed_workspace.downgrade();

    window.spawn(cx, async move |cx| {
        let opened = cx
            .background_spawn({
                let lifecycle = lifecycle.clone();
                let id = id.clone();
                async move {
                    let (workspace, state) = lifecycle.open_workspace(&id).await?;
                    // **The daemon is the restore path.** A workspace whose
                    // session is alive has an arrangement stored beside it, and
                    // opening means building that and attaching to what it
                    // names — never spawning (operator spec, 2026-08-04). Only
                    // when there is no stored layout to build does the
                    // single-terminal attach-or-create below run, which is what
                    // a workspace being opened for the first time is.
                    let stored = matches!(state, SessionState::Alive)
                        .then(|| lifecycle.open_workspace_layout(&workspace))
                        .transpose()
                        .unwrap_or_else(|error| {
                            log::warn!(
                                "no stored layout for workspace {}, opening a terminal instead: {error:#}",
                                workspace.id
                            );
                            None
                        });
                    let attached = match (&stored, state) {
                        (Some(_), _) => None,
                        // `Unknown` cannot come out of a probe — only a
                        // reconciliation whose host failed produces it — and it
                        // would mean "we do not know", which is never grounds
                        // for attach-or-*create*.
                        (None, SessionState::Dead | SessionState::Unknown) => None,
                        (None, SessionState::Alive | SessionState::NeverCreated) => {
                            Some(lifecycle.attach_command(&workspace)?)
                        }
                    };
                    anyhow::Ok((workspace, state, stored, attached))
                }
            })
            .await;

        let outcome = match opened {
            Ok((workspace, state, stored, attached)) => {
                // The Zed window *is* the workspace view: make the repository a
                // visible worktree so the file tree and git panel follow the
                // selection. Additive and never fatal — see
                // [`crate::workspace_view`].
                ensure_repository_worktree(&zed_workspace, &workspace, cx).await;
                name_window_after_workspace(&zed_workspace, &workspace, cx)?;

                if let Some(stored) = stored {
                    // A window that is already showing this workspace is
                    // *focused*, not rebuilt: its sync takes the fresh read —
                    // rendered only if its revision is news — and the build
                    // below, which would tear down and re-attach every
                    // terminal on screen, is skipped.
                    let already_showing = cx
                        .update(|_, cx| {
                            AdeLayouts::catch_up_if_showing(
                                zed_workspace.entity_id(),
                                &workspace,
                                &stored,
                                cx,
                            )
                        })
                        .unwrap_or(false);
                    if already_showing {
                        Ok(())
                    } else {
                        build_layout(&zed_workspace, &workspace, stored, cx).await
                    }
                } else {
                    match attached {
                        Some(attached) => {
                            let opened =
                                attach_terminal(&zed_workspace, &workspace, attached, cx).await;
                            match (opened, state) {
                                // The pane's own attach-or-create is what brought
                                // the session into being, so the registry has to be
                                // told the name it now points at — otherwise the dot
                                // would keep reading muted over a running session.
                                (Ok(()), SessionState::NeverCreated) => {
                                    cx.background_spawn(async move {
                                        lifecycle.record_attached_session(&id).await.map(|_| ())
                                    })
                                    .await
                                }
                                (opened, _) => opened,
                            }
                        }
                        // Dead: the caller renders its "gone" affordance and waits.
                        // The file tree still updated above, so the repository is
                        // browsable while the session is not.
                        None => Ok(()),
                    }
                }
            }
            Err(error) => Err(error),
        };

        cx.update(|_, cx| {
            AdeWorkspaceStore::global(cx).update(cx, |store, cx| store.refresh(cx));
        })
        .ok();

        outcome
    })
}

/// Whether this window's persistent-workspace binding belongs to the exact
/// worktree row the user acted on.
pub fn can_reset_workspace_sessions(
    zed_workspace: &Entity<Workspace>,
    repository_path: &Path,
    remote_host: Option<&str>,
    cx: &App,
) -> bool {
    bound_workspace_for_worktree(zed_workspace.entity_id(), repository_path, remote_host, cx)
        .is_some()
}

/// Kills every persistent session in the workspace shown by this window and
/// attaches one fresh session without restarting the host daemon.
///
/// The window binding identifies the selected workspace; only other registry
/// rows for that exact host and repository root share its recovery scope.
pub fn kill_and_recreate_workspace_sessions(
    zed_workspace: &Entity<Workspace>,
    repository_path: PathBuf,
    remote_host: Option<String>,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<()>> {
    let Some(id) = bound_workspace_for_worktree(
        zed_workspace.entity_id(),
        &repository_path,
        remote_host.as_deref(),
        cx,
    )
    .cloned() else {
        return Task::ready(Err(anyhow::anyhow!(
            "this worktree is not the persistent workspace attached to this window"
        )));
    };
    let lifecycle = crate::lifecycle_service(cx);
    let zed_workspace = zed_workspace.downgrade();

    window.spawn(cx, async move |cx| {
        let pane =
            zed_workspace.read_with(cx, |workspace, _| workspace.active_pane().downgrade())?;
        pane.update(cx, |pane, cx| pane.begin_pending_item(cx))?;
        let outcome = async {
            let entity_id = zed_workspace.entity_id();
            cx.update(|_, cx| AdeLayouts::forget_window(entity_id, cx))?;
            close_center_terminals(&zed_workspace, cx).await?;
            let (ade_workspace, attached) = cx
                .background_spawn(async move { lifecycle.reset_workspace_sessions(&id).await })
                .await?;
            cx.update(|_, cx| AdeLayouts::forget_window(entity_id, cx))?;
            close_center_terminals(&zed_workspace, cx).await?;
            attach_terminal(&zed_workspace, &ade_workspace, attached, cx).await
        }
        .await;
        if outcome.is_err() {
            rollback_layout_ownership(&zed_workspace, false, cx);
        }
        pane.update(cx, |pane, cx| pane.end_pending_item(cx))
            .log_err();

        cx.update(|_, cx| {
            AdeWorkspaceStore::global(cx).update(cx, |store, cx| store.refresh(cx));
        })
        .ok();
        outcome
    })
}

/// Removes the terminal views before the backend resets their sessions,
/// without disturbing editor tabs in the same panes.
async fn close_center_terminals(
    zed_workspace: &WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let terminals = zed_workspace.read_with(cx, |workspace, cx| {
        workspace
            .center()
            .panes()
            .into_iter()
            .flat_map(|pane| {
                let pane = pane.clone();
                let item_ids = pane
                    .read(cx)
                    .items()
                    .filter(|item| {
                        item.downcast::<terminal_view::TerminalView>().is_some()
                            || item.downcast::<MissingTab>().is_some_and(|placeholder| {
                                matches!(
                                    placeholder.read(cx).tab(),
                                    ade_session::Tab::Terminal { .. }
                                )
                            })
                    })
                    .map(|item| item.item_id())
                    .collect::<Vec<_>>();
                item_ids
                    .into_iter()
                    .map(move |item_id| (pane.clone(), item_id))
            })
            .collect::<Vec<_>>()
    })?;

    for (pane, item_id) in terminals {
        let close = cx.update(|window, cx| {
            pane.update(cx, |pane, cx| {
                pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
            })
        })?;
        close.await?;
    }
    Ok(())
}

/// Builds the window's whole centre from the stored layout, then keeps the two
/// in step.
///
/// The layout half of opening a workspace: panes, splits and tabs as the daemon
/// holds them, every terminal tab **attached** to the session it names. The
/// sync installed afterwards is what pushes the user's own rearranging back —
/// and what applies another client's.
async fn build_layout(
    zed_workspace: &WeakEntity<Workspace>,
    ade_workspace: &AdeWorkspace,
    stored: WorkspaceLayout,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let zed_workspace = zed_workspace.upgrade().context("the window is gone")?;
    let preserve_existing_sync =
        cx.update(|_, cx| AdeLayouts::is_showing(zed_workspace.entity_id(), ade_workspace, cx))?;
    zed_workspace.update_in(cx, |zed_workspace, window, cx| {
        zed_workspace.set_ade_owns_layout(window, cx)
    })?;

    let outcome = async {
        let render = zed_workspace.update_in(cx, |_, window, cx| {
            render_layout(
                &zed_workspace,
                ade_workspace,
                stored.layout.clone(),
                window,
                cx,
            )
        })?;
        render.await?;

        // **Not `zed_workspace.update_in`.** `install` reads the workspace's panes
        // to seed its item → session map, so calling it with the workspace already
        // leased is a double lease: GPUI panics with "cannot read
        // workspace::Workspace while it is already being updated", and on Windows
        // that unwinds into the nounwind window procedure and aborts the whole
        // process. `cx.update` hands out the window and the app without leasing
        // anything. See [`AdeLayouts::install`].
        cx.update(|window, cx| {
            AdeLayouts::install(
                &zed_workspace,
                ade_workspace.clone(),
                stored.layout,
                stored.rev,
                window,
                cx,
            );
        })?;
        Ok(())
    }
    .await;
    if outcome.is_err() {
        rollback_layout_ownership(&zed_workspace.downgrade(), preserve_existing_sync, cx);
    }
    outcome
}

/// Opens (or refocuses) the workspace's centre terminal on an attach the caller
/// already ran, and leaves the window syncing.
///
/// The other half of opening a workspace: there was no stored arrangement to
/// build, so the window *is* the arrangement — one terminal on the session the
/// attach landed on. Marking, opening and syncing are all here rather than at
/// the three call sites, because a window that has any of them and not the
/// others is exactly the state this converges away from.
pub(crate) async fn attach_terminal(
    zed_workspace: &WeakEntity<Workspace>,
    ade_workspace: &AdeWorkspace,
    attached: Attached,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let preserve_existing_sync =
        cx.update(|_, cx| AdeLayouts::is_showing(zed_workspace.entity_id(), ade_workspace, cx))?;
    // Before the terminal lands, for the reason [`open_workspace_session`]
    // gives: a serialize that fires mid-open must not write this centre.
    zed_workspace.update_in(cx, |zed_workspace, window, cx| {
        zed_workspace.set_ade_owns_layout(window, cx)
    })?;

    let attach = zed_workspace.update_in(cx, |zed_workspace, window, cx| {
        open_workspace_terminal(zed_workspace, ade_workspace, attached, window, cx)
    })?;
    if let Err(error) = attach.await {
        rollback_layout_ownership(zed_workspace, preserve_existing_sync, cx);
        return Err(error);
    }
    install_sync_after_attach(zed_workspace, ade_workspace, preserve_existing_sync, cx).await
}

async fn install_sync_after_attach(
    zed_workspace: &WeakEntity<Workspace>,
    ade_workspace: &AdeWorkspace,
    preserve_existing_sync: bool,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let outcome = install_sync(zed_workspace, ade_workspace, cx).await;
    if outcome.is_err() {
        rollback_layout_ownership(zed_workspace, preserve_existing_sync, cx);
    }
    outcome
}

fn rollback_layout_ownership(
    zed_workspace: &WeakEntity<Workspace>,
    preserve_existing_sync: bool,
    cx: &mut AsyncWindowContext,
) {
    if preserve_existing_sync {
        return;
    }
    if let Some(id) = zed_workspace
        .upgrade()
        .map(|workspace| workspace.entity_id())
    {
        cx.update(|_, cx| AdeLayouts::forget_window(id, cx))
            .log_err();
    }
    zed_workspace
        .update_in(cx, |zed_workspace, window, cx| {
            zed_workspace.clear_ade_owns_layout(window, cx)
        })
        .log_err();
}

/// Starts the layout sync for a window that was opened without a stored
/// arrangement to build from.
///
/// **Every window holding a daemon terminal syncs**, or a terminal added to it
/// later would never reach the document and would be gone with the window. The
/// terminal above carries its session id on its spawn task (see
/// [`crate::session_task_id`]), so the first capture names it — which is also
/// how a recreated session replaces the dead one a stale stored document still
/// names, since [`AdeLayouts::install`] pushes what the window actually shows.
///
/// Failure is fatal to the attach operation: leaving a daemon-owned window
/// without a sync would make later terminal actions fail and let its centre
/// disappear on restart. The caller rolls ownership back before surfacing the
/// error.
async fn install_sync(
    zed_workspace: &WeakEntity<Workspace>,
    ade_workspace: &AdeWorkspace,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let lifecycle = cx.update(|_, cx| crate::lifecycle_service(cx))?;
    let stored = cx
        .background_spawn({
            let ade_workspace = ade_workspace.clone();
            // Blocking: one round trip to the backend.
            async move { lifecycle.open_workspace_layout(&ade_workspace) }
        })
        .await;
    let stored = stored.with_context(|| {
        format!(
            "workspace {} stores no layout after its terminal attached",
            ade_workspace.id
        )
    })?;
    let zed_workspace = zed_workspace.upgrade().context("the window is gone")?;
    // `cx.update`, never `zed_workspace.update_in` — `install` reads the
    // window's panes, and a read nested in an update is the double lease GPUI
    // panics on. See [`build_layout`].
    cx.update(|window, cx| {
        AdeLayouts::install(
            &zed_workspace,
            ade_workspace.clone(),
            stored.layout,
            stored.rev,
            window,
            cx,
        );
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DaemonEvent, GlobalLifecycleService, SessionBackend, SessionId, SessionInfo, SessionSpec,
        StatusDelivery, WorkspaceLifecycleService,
    };
    use anyhow::bail;
    use gpui::{TestAppContext, VisualTestContext};
    use std::{path::Path, sync::Arc};

    struct MissingLayoutBackend;

    impl SessionBackend for MissingLayoutBackend {
        fn create(&self, _spec: &SessionSpec) -> Result<SessionId> {
            bail!("not used")
        }

        fn list(&self) -> Result<Vec<SessionInfo>> {
            Ok(Vec::new())
        }

        fn exists(&self, _id: &SessionId) -> Result<bool> {
            Ok(false)
        }

        fn attach(&self, _spec: &SessionSpec) -> Result<Attached> {
            bail!("not used")
        }

        fn detach(&self, _id: &SessionId) -> Result<()> {
            Ok(())
        }

        fn kill(&self, _id: &SessionId) -> Result<()> {
            Ok(())
        }

        fn status_delivery(&self) -> StatusDelivery {
            StatusDelivery::Push
        }

        fn subscribe_events(&self) -> Result<smol::channel::Receiver<DaemonEvent>> {
            let (_sender, receiver) = smol::channel::unbounded();
            Ok(receiver)
        }

        fn open_workspace(&self, _workspace_id: &str) -> Result<WorkspaceLayout> {
            bail!("layout unavailable")
        }
    }

    async fn test_window(
        name: &'static str,
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, AdeWorkspace, VisualTestContext) {
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
        let project = project::Project::test(fs, [Path::new("/repo")], cx).await;
        let (workspace, window) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));

        let registry = crate::AdeWorkspaceRegistry::open_test_db(name).await;
        let ade_workspace = AdeWorkspace::new("main", "repo", "/repo");
        registry
            .create_workspace(ade_workspace.clone())
            .await
            .expect("test workspace should be registered");
        let lifecycle = Arc::new(WorkspaceLifecycleService::with_backend(
            registry,
            Arc::new(MissingLayoutBackend),
        ));
        window.update(|_, cx| cx.set_global(GlobalLifecycleService(lifecycle)));

        (workspace, ade_workspace, window.clone())
    }

    fn failed_attach() -> Attached {
        Attached {
            session_id: "missing-session".to_owned(),
            argv: Vec::new(),
        }
    }

    async fn attach(
        workspace: &Entity<Workspace>,
        ade_workspace: &AdeWorkspace,
        attached: Attached,
        cx: &mut VisualTestContext,
    ) -> Result<()> {
        let task = cx.update(|window, cx| {
            window.spawn(cx, {
                let workspace = workspace.downgrade();
                let ade_workspace = ade_workspace.clone();
                async move |cx| attach_terminal(&workspace, &ade_workspace, attached, cx).await
            })
        });
        task.await
    }

    #[gpui::test]
    async fn recovery_removes_terminal_placeholders_but_keeps_editor_placeholders(
        cx: &mut TestAppContext,
    ) {
        let (workspace, _ade_workspace, mut window) =
            test_window("recovery_removes_terminal_placeholders", cx).await;
        let pane = workspace.read_with(&window, |workspace, _| workspace.active_pane().clone());
        window.update(|window, cx| {
            let terminal = cx.new(|cx| MissingTab::session("dead-session", cx));
            let editor = cx.new(|cx| MissingTab::file("/repo/missing.rs", cx));
            pane.update(cx, |pane, cx| {
                pane.add_item(Box::new(terminal), false, false, None, window, cx);
                pane.add_item(Box::new(editor), false, false, None, window, cx);
            });
        });

        let task = window.update(|window, cx| {
            window.spawn(cx, {
                let workspace = workspace.downgrade();
                async move |cx| close_center_terminals(&workspace, cx).await
            })
        });
        task.await.expect("placeholder cleanup should succeed");

        let remaining = pane.read_with(&window, |pane, cx| {
            pane.items()
                .filter_map(|item| item.downcast::<MissingTab>())
                .map(|placeholder| placeholder.read(cx).tab().clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            remaining,
            vec![ade_session::Tab::Editor {
                path: "/repo/missing.rs".to_owned()
            }]
        );
    }

    #[gpui::test]
    async fn a_bound_workspace_can_reset_without_layout_ownership(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut window) =
            test_window("bound_workspace_can_reset_without_layout_ownership", cx).await;
        assert!(!window.update(|_, cx| {
            can_reset_workspace_sessions(&workspace, Path::new("/repo"), None, cx)
        }));
        let bind = window.update(|window, cx| {
            window.spawn(cx, {
                let workspace = workspace.downgrade();
                let ade_workspace = ade_workspace.clone();
                async move |cx| name_window_after_workspace(&workspace, &ade_workspace, cx)
            })
        });
        bind.await.expect("the workspace should bind");

        assert!(!workspace.read_with(&window, |workspace, _| workspace.ade_owns_layout()));
        assert!(window.update(|_, cx| {
            can_reset_workspace_sessions(&workspace, Path::new("/repo"), None, cx)
        }));
        assert!(!window.update(|_, cx| {
            can_reset_workspace_sessions(&workspace, Path::new("/repo/sibling"), None, cx)
        }));
    }

    #[gpui::test]
    async fn reset_failure_does_not_leave_the_killed_terminal_visible(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut window) =
            test_window("reset_failure_does_not_leave_killed_terminal", cx).await;
        let bind = window.update(|window, cx| {
            window.spawn(cx, {
                let workspace = workspace.downgrade();
                async move |cx| name_window_after_workspace(&workspace, &ade_workspace, cx)
            })
        });
        bind.await.expect("the workspace should bind");

        let pane = workspace.read_with(&window, |workspace, _| workspace.active_pane().clone());
        window.update(|window, cx| {
            let terminal = cx.new(|cx| MissingTab::session("old-session", cx));
            pane.update(cx, |pane, cx| {
                pane.add_item(Box::new(terminal), false, false, None, window, cx);
            });
        });

        let reset = window.update(|window, cx| {
            kill_and_recreate_workspace_sessions(
                &workspace,
                PathBuf::from("/repo"),
                None,
                window,
                cx,
            )
        });
        let error = reset
            .await
            .expect_err("the test backend deliberately refuses the replacement session");
        assert!(format!("{error:#}").contains("creating session"));
        assert!(pane.read_with(&window, |pane, cx| {
            pane.items().all(|item| {
                !item.downcast::<MissingTab>().is_some_and(|placeholder| {
                    matches!(
                        placeholder.read(cx).tab(),
                        ade_session::Tab::Terminal { .. }
                    )
                })
            })
        }));
        assert!(!pane.read_with(&window, |pane, _| pane.has_pending_item()));
        assert!(
            window.update(|_, cx| {
                can_reset_workspace_sessions(&workspace, Path::new("/repo"), None, cx)
            }),
            "a failed recovery must remain retryable"
        );
    }

    #[gpui::test(iterations = 10)]
    async fn failed_terminal_attach_returns_the_center_to_zed(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut window) =
            test_window("failed_terminal_attach_returns_center", cx).await;

        let error = attach(&workspace, &ade_workspace, failed_attach(), &mut window)
            .await
            .expect_err("the empty attach command must fail");
        assert!(format!("{error:#}").contains("empty"));
        assert!(!workspace.read_with(&window, |workspace, _| workspace.ade_owns_layout()));
    }

    #[gpui::test(iterations = 10)]
    async fn failed_attach_removes_the_previous_workspace_sync(cx: &mut TestAppContext) {
        let (workspace, old_workspace, mut window) =
            test_window("failed_attach_removes_previous_sync", cx).await;
        let old_layout = ade_session::LayoutDoc::default();
        window.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.set_ade_owns_layout(window, cx)
            });
            AdeLayouts::install(
                &workspace,
                old_workspace.clone(),
                old_layout.clone(),
                1,
                window,
                cx,
            );
        });

        let replacement = AdeWorkspace::new("other", "repo", "/repo");
        attach(&workspace, &replacement, failed_attach(), &mut window)
            .await
            .expect_err("the replacement attach must fail");

        assert!(!workspace.read_with(&window, |workspace, _| workspace.ade_owns_layout()));
        assert!(!window.update(|_, cx| {
            AdeLayouts::catch_up_if_showing(
                workspace.entity_id(),
                &old_workspace,
                &WorkspaceLayout {
                    layout: old_layout,
                    rev: 2,
                },
                cx,
            )
        }));
    }

    #[gpui::test(iterations = 10)]
    async fn failed_retry_preserves_the_same_workspace_sync(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut window) =
            test_window("failed_retry_preserves_same_sync", cx).await;
        window.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.set_ade_owns_layout(window, cx)
            });
            AdeLayouts::install(
                &workspace,
                ade_workspace.clone(),
                ade_session::LayoutDoc::default(),
                1,
                window,
                cx,
            );
        });

        attach(&workspace, &ade_workspace, failed_attach(), &mut window)
            .await
            .expect_err("the retry attach must fail");

        assert!(workspace.read_with(&window, |workspace, _| workspace.ade_owns_layout()));
        assert!(
            window.update(|_, cx| {
                AdeLayouts::is_showing(workspace.entity_id(), &ade_workspace, cx)
            })
        );
    }

    #[gpui::test(iterations = 10)]
    async fn post_attach_layout_failure_does_not_leave_an_unsynced_owner(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut window) =
            test_window("post_attach_layout_failure", cx).await;
        window.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.set_ade_owns_layout(window, cx)
            })
        });
        let task = window.update(|window, cx| {
            window.spawn(cx, {
                let workspace = workspace.downgrade();
                let ade_workspace = ade_workspace.clone();
                async move |cx| {
                    install_sync_after_attach(&workspace, &ade_workspace, false, cx).await
                }
            })
        });
        let error = task
            .await
            .expect_err("installing sync must surface the missing layout");

        assert!(format!("{error:#}").contains("layout unavailable"));
        assert!(!workspace.read_with(&window, |workspace, _| workspace.ade_owns_layout()));
        assert!(!window.update(|_, cx| {
            AdeLayouts::catch_up_if_showing(
                workspace.entity_id(),
                &ade_workspace,
                &WorkspaceLayout {
                    layout: ade_session::LayoutDoc::default(),
                    rev: 1,
                },
                cx,
            )
        }));
    }
}
