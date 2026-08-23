//! What "opening a workspace" does to the Zed window around the terminal.
//!
//! **An ADE workspace is the whole main-window layout** — the project's file
//! tree on the left, the session's terminal in the middle — not a terminal tab
//! dropped into whatever project happened to be showing (operator ruling,
//! 2026-08-04). That supersedes the "additive, no project swap, no new window"
//! decision of 2026-08-01 *for the open flow*.
//!
//! **The switch is not this module's job.** The caller must first activate a
//! workspace on the same remote host and repository root. The binding below
//! verifies that precondition so a workspace-management view cannot replace an
//! unrelated project's centre and identity.
//!
//! **What is left here is the additive top-up.** For a *local* workspace whose
//! checkout is not among the resolved project's worktrees — the paths did not
//! match exactly, or the user opened a parent directory — this ensures
//! `repository_path` is a visible worktree so the file tree and git panel cover
//! it. It never removes a worktree, never swaps a project, and never opens a
//! window; the switch above has already decided those. A *remote* workspace
//! contributes nothing here: its file tree comes from the remote project the
//! switch opened, and adding its path as a local worktree would open some
//! unrelated directory here — see [`worktree_path`].
//!
//! **Deliberately not built, deferred past the MVP:**
//!
//! - *Exclusive re-scoping* — dropping the worktrees this top-up added once the
//!   user selects a different workspace in the same project.
//! - *Per-workspace layout swap* — remembering and restoring each workspace's
//!   own pane/dock arrangement on selection.
//!
//! Neither was overlooked. Both were considered and consciously left out; a
//! future change that adds them should say so and delete these paragraphs.
//!
//! **Never fatal.** Terminal state outranks everything here. If the worktree
//! cannot be added — the path is gone, the project refuses it — this logs and
//! returns, and the caller goes on to attach the terminal. A missing file tree
//! is a degraded window; a missing terminal is a lost session.

use crate::{AdeWorkspace, WorkspaceId};
use anyhow::{Context as _, Result, ensure};
use gpui::{App, AppContext as _, AsyncWindowContext, EntityId, Global, SharedString, WeakEntity};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use util::ResultExt as _;
use workspace::Workspace;

/// The path this workspace should contribute to the window's project, if any.
///
/// `None` for a remote workspace: its `repository_path` names a checkout on
/// another host, and adding it as a *local* worktree would either fail or —
/// worse — silently open some unrelated local directory that happens to share
/// the path.
///
/// A remote workspace does now get a file tree, but not from here: giving one
/// means telling Zed's *project* which host the path belongs to, which is its
/// remote-project machinery. The ledger drives that before calling in (see the
/// module docs), so by this point the tree is already scoped and there is
/// nothing local left to add.
fn worktree_path(ade_workspace: &AdeWorkspace) -> Option<&Path> {
    (!ade_workspace.is_remote()).then_some(ade_workspace.repository_path.as_path())
}

/// Ensures the workspace's repository is a visible worktree of the window's
/// project, so the file tree and git panel are scoped to it.
///
/// Idempotent by construction: an exact visible worktree is reused, and a
/// covering parent does not prevent the repository itself from being added.
///
/// Failures are logged and swallowed on purpose — see the module docs.
pub(crate) async fn ensure_repository_worktree(
    zed_workspace: &WeakEntity<Workspace>,
    ade_workspace: &AdeWorkspace,
    cx: &mut AsyncWindowContext,
) {
    let Some(path) = worktree_path(ade_workspace).map(Path::to_path_buf) else {
        return;
    };

    let ensure_exact = zed_workspace.update(cx, |zed_workspace, cx| {
        zed_workspace.project().update(cx, |project, cx| {
            if let Some(worktree) = project
                .visible_worktrees(cx)
                .find(|worktree| worktree.read(cx).abs_path().as_ref() == path.as_path())
            {
                gpui::Task::ready(Ok(worktree))
            } else {
                project.create_worktree(path, true, cx)
            }
        })
    });

    if let Some(ensure_exact) = ensure_exact.log_err() {
        ensure_exact.await.log_err();
    }
}

/// Which ADE workspace each window is currently showing.
///
/// A window's header names a workspace, and a later rename has to find the
/// window that header belongs to. The displayed *name* cannot answer that: an
/// un-renamed workspace is named for its project
/// (`WorkspaceLifecycleService`'s `display_name_for`), so every workspace of one
/// project carries the same one and matching on it would retitle a window
/// showing a sibling.
///
/// Keyed by the Zed workspace's entity id and dropped when that workspace is
/// released, since GPUI reuses entity ids and a stale row would answer for the
/// window that inherited one.
struct BoundWorkspace {
    id: WorkspaceId,
    repository_path: PathBuf,
    remote_host: Option<String>,
    worktree_id: Option<project::WorktreeId>,
    scope_generation: Arc<AtomicU64>,
    scope_updates: Arc<smol::lock::Mutex<()>>,
}

#[derive(Default)]
struct BoundWorkspaces(HashMap<EntityId, BoundWorkspace>);

impl Global for BoundWorkspaces {}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        let project = workspace.project().clone();
        cx.subscribe(&project, |_, _, event, cx| {
            if !matches!(event, project::Event::WorktreePathsChanged { .. }) {
                return;
            }

            let Some((id, worktree_id, generation, updates)) = cx
                .try_global::<BoundWorkspaces>()
                .and_then(|bindings| bindings.0.get(&cx.entity_id()))
                .and_then(|binding| {
                    Some((
                        binding.id.clone(),
                        binding.worktree_id?,
                        binding.scope_generation.clone(),
                        binding.scope_updates.clone(),
                    ))
                })
            else {
                return;
            };
            let revision = generation.fetch_add(1, Ordering::AcqRel) + 1;
            let lifecycle = crate::lifecycle_service(cx);
            cx.spawn(async move |this, cx| {
                let poll = std::time::Duration::from_millis(250);
                let deadline = std::time::Duration::from_secs(120);
                let mut waited = std::time::Duration::ZERO;
                let (repository_path, project_id, project_identity) = loop {
                    if generation.load(Ordering::Acquire) != revision {
                        return;
                    }
                    let scope = this
                        .update(cx, |workspace, cx| {
                            if !workspace.project_group_identity_is_known(cx) {
                                return None;
                            }
                            let project = workspace.project().read(cx);
                            let worktree = project.worktree_for_id(worktree_id, cx)?;
                            let repository_path = worktree.read(cx).abs_path().to_path_buf();
                            let project_group_key = project.project_group_key(cx);
                            let project_identity =
                                project_group_key.path_list().serialize().paths;
                            (!project_identity.is_empty()).then(|| {
                                (
                                    repository_path,
                                    project_group_key
                                        .display_name(&Default::default())
                                        .to_string(),
                                    project_identity,
                                )
                            })
                        })
                        .ok()
                        .flatten();
                    if let Some(scope) = scope {
                        break scope;
                    }
                    if waited >= deadline {
                        log::warn!(
                            "ADE project identity did not settle after a bound worktree moved"
                        );
                        return;
                    }
                    cx.background_executor().timer(poll).await;
                    waited += poll;
                };
                let update = cx
                    .background_spawn({
                        let id = id.clone();
                        let repository_path = repository_path.clone();
                        let generation = generation.clone();
                        async move {
                            let _guard = updates.lock().await;
                            if generation.load(Ordering::Acquire) != revision {
                                return Ok(None);
                            }
                            lifecycle
                                .update_workspace_repository_scope(
                                    &id,
                                    repository_path,
                                    &project_id,
                                    &project_identity,
                                )
                                .await
                                .map(Some)
                        }
                    })
                    .await;
                match update {
                    Ok(Some(updated)) => {
                        this.update(cx, |_, cx| {
                            let entity_id = cx.entity_id();
                            if let Some(binding) = cx
                                .default_global::<BoundWorkspaces>()
                                .0
                                .get_mut(&entity_id)
                                && binding.id == updated.id
                                && Arc::ptr_eq(&binding.scope_generation, &generation)
                                && generation.load(Ordering::Acquire) == revision
                            {
                                binding.repository_path = updated.repository_path;
                            }
                            if let Some(store) = crate::AdeWorkspaceStore::try_global(cx) {
                                store.update(cx, |store, cx| store.refresh(cx));
                            }
                        })
                        .ok();
                    }
                    Ok(None) => {}
                    Err(error) => log::warn!(
                        "updating the bound ADE workspace after its worktree moved failed: {error:#}"
                    ),
                }
            })
            .detach();
        })
        .detach();
    })
    .detach();
}

/// The ADE workspace `zed_workspace` was last opened onto.
pub(crate) fn bound_workspace(zed_workspace: EntityId, cx: &App) -> Option<&WorkspaceId> {
    cx.try_global::<BoundWorkspaces>()?
        .0
        .get(&zed_workspace)
        .map(|workspace| &workspace.id)
}

pub(crate) fn clear_window_binding(zed_workspace: EntityId, cx: &mut App) {
    if cx.has_global::<BoundWorkspaces>() {
        cx.global_mut::<BoundWorkspaces>().0.remove(&zed_workspace);
    }
}

/// The bound ADE workspace, only when it is the exact worktree row the user
/// acted on. One Zed workspace can expose several repository roots, so its last
/// binding alone is not enough to select a destructive action safely.
pub(crate) fn bound_workspace_for_worktree<'a>(
    zed_workspace: EntityId,
    repository_path: &Path,
    remote_host: Option<&str>,
    cx: &'a App,
) -> Option<&'a WorkspaceId> {
    let workspace = cx.try_global::<BoundWorkspaces>()?.0.get(&zed_workspace)?;
    (workspace.repository_path == repository_path
        && workspace.remote_host.as_deref() == remote_host)
        .then_some(&workspace.id)
}

/// Binds the window to the workspace and puts its name in the header — the OS
/// title bar and the in-app one both — in place of the checkout folder Zed would
/// otherwise name the window after.
///
/// The name is the only handle the user has on a workspace, and a checkout can
/// carry several of them, so the folder name identifies nothing. Applies to
/// remote workspaces too, which contribute no worktree at all.
///
/// Every open re-asserts both, which is also what repairs a header left stale by
/// a rename that happened in another window.
pub(crate) fn name_window_after_workspace(
    zed_workspace: &WeakEntity<Workspace>,
    ade_workspace: &AdeWorkspace,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    if ade_workspace.is_remote() {
        let (remote_host, roots) = zed_workspace.read_with(cx, |zed_workspace, cx| {
            let project = zed_workspace.project().read(cx);
            let remote_host = project
                .remote_connection_options(cx)
                .as_ref()
                .and_then(crate::destination_for);
            let roots = project
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect::<Vec<_>>();
            (remote_host, roots)
        })?;
        ensure!(
            remote_host == ade_workspace.remote_host
                && roots.contains(&ade_workspace.repository_path),
            "workspace {} belongs to {} at {}, but this window is scoped to {} at {:?}",
            ade_workspace.id,
            ade_workspace
                .remote_host
                .as_deref()
                .unwrap_or("a local host"),
            ade_workspace.repository_path.display(),
            remote_host.as_deref().unwrap_or("a local host"),
            roots
        );
    }

    let name = SharedString::from(ade_workspace.name.clone());
    zed_workspace.update_in(cx, |zed_workspace, window, cx| {
        zed_workspace.set_window_title_override(Some(name), window, cx);
    })?;

    let zed_workspace = zed_workspace.upgrade().context("the window is gone")?;
    let worktree_id = zed_workspace.read_with(cx, |zed_workspace, cx| {
        zed_workspace
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .find(|worktree| {
                worktree.read(cx).abs_path().as_ref() == ade_workspace.repository_path.as_path()
            })
            .map(|worktree| worktree.read(cx).id())
    });
    let binding = BoundWorkspace {
        id: ade_workspace.id.clone(),
        repository_path: ade_workspace.repository_path.clone(),
        remote_host: ade_workspace.remote_host.clone(),
        worktree_id,
        scope_generation: Arc::new(AtomicU64::new(0)),
        scope_updates: Arc::new(smol::lock::Mutex::new(())),
    };
    cx.update(|_, cx| {
        let entity_id = zed_workspace.entity_id();
        // One release observer per window, registered with its first binding;
        // re-opening only replaces the id that binding points at.
        let previous = cx
            .default_global::<BoundWorkspaces>()
            .0
            .insert(entity_id, binding);
        if let Some(previous) = previous.as_ref() {
            previous.scope_generation.fetch_add(1, Ordering::Release);
        } else {
            cx.observe_release(&zed_workspace, move |_, cx| {
                cx.default_global::<BoundWorkspaces>().0.remove(&entity_id);
            })
            .detach();
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::{path::PathBuf, time::Duration};

    const ROOT: &str = "/repos/zed";
    const MOVED_ROOT: &str = "/repos/zed-renamed";

    struct RegistryOnlyBackend;

    impl crate::SessionBackend for RegistryOnlyBackend {
        fn create(
            &self,
            _spec: &crate::SessionSpec,
            _expected_daemon_id: Option<&str>,
        ) -> Result<crate::SessionId> {
            anyhow::bail!("the registry-only test backend cannot create sessions")
        }

        fn list(&self) -> Result<Vec<crate::SessionInfo>> {
            Ok(Vec::new())
        }

        fn exists(
            &self,
            _id: &crate::SessionId,
            _expected_daemon_id: Option<&str>,
        ) -> Result<bool> {
            Ok(false)
        }

        fn attach(
            &self,
            _spec: &crate::SessionSpec,
            _expected_daemon_id: Option<&str>,
        ) -> Result<crate::Attached> {
            anyhow::bail!("the registry-only test backend cannot attach sessions")
        }

        fn detach(&self, _id: &crate::SessionId) -> Result<()> {
            Ok(())
        }

        fn kill(&self, _id: &crate::SessionId, _expected_daemon_id: Option<&str>) -> Result<()> {
            Ok(())
        }

        fn status_delivery(&self) -> crate::StatusDelivery {
            crate::StatusDelivery::Poll {
                interval: Duration::from_secs(1),
            }
        }
    }

    /// A window over a one-file project, and the ADE workspace it stands for.
    async fn test_window_with_project_root(
        cx: &mut TestAppContext,
        project_root: &str,
    ) -> (Entity<Workspace>, AdeWorkspace, VisualTestContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            super::init(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(ROOT, serde_json::json!({ "a.rs": "fn a() {}\n" }))
            .await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let project = project::Project::test(fs, [project_root.as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        (
            workspace,
            AdeWorkspace::new("Vector DB spike", "zed", ROOT),
            cx.clone(),
        )
    }

    async fn test_window(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, AdeWorkspace, VisualTestContext) {
        test_window_with_project_root(cx, ROOT).await
    }

    /// [`name_window_after_workspace`] as the open paths reach it: from a task
    /// spawned on the window, never from inside a window update.
    async fn bind(
        workspace: &Entity<Workspace>,
        ade_workspace: &AdeWorkspace,
        cx: &mut VisualTestContext,
    ) -> Result<()> {
        let task = cx.update(|window, cx| {
            window.spawn(cx, {
                let handle = workspace.downgrade();
                let ade_workspace = ade_workspace.clone();
                async move |cx| name_window_after_workspace(&handle, &ade_workspace, cx)
            })
        });
        task.await
    }

    #[gpui::test]
    async fn test_the_window_is_named_after_the_workspace(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        assert_eq!(cx.window_title().as_deref(), Some("zed"));

        bind(&workspace, &ade_workspace, &mut cx)
            .await
            .expect("the matching workspace should bind");

        assert_eq!(cx.window_title().as_deref(), Some("Vector DB spike"));
        assert_eq!(
            workspace.read_with(&cx, |workspace, _| workspace
                .window_title_override()
                .cloned()),
            Some("Vector DB spike".into())
        );
        assert_eq!(
            cx.update(|_, cx| bound_workspace(workspace.entity_id(), cx).cloned()),
            Some(ade_workspace.id.clone())
        );
    }

    #[gpui::test]
    async fn test_ensure_repository_worktree_adds_an_exact_root_below_a_visible_parent(
        cx: &mut TestAppContext,
    ) {
        let (workspace, ade_workspace, mut cx) = test_window_with_project_root(cx, "/repos").await;
        let project = workspace.read_with(&cx, |workspace, _| workspace.project().clone());
        assert_eq!(
            project.read_with(&cx, |project, cx| {
                project
                    .visible_worktrees(cx)
                    .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                    .collect::<Vec<_>>()
            }),
            vec![PathBuf::from("/repos")]
        );

        let ensure = cx.update(|window, cx| {
            window.spawn(cx, {
                let workspace = workspace.downgrade();
                let ade_workspace = ade_workspace.clone();
                async move |cx| {
                    ensure_repository_worktree(&workspace, &ade_workspace, cx).await;
                }
            })
        });
        ensure.await;
        bind(&workspace, &ade_workspace, &mut cx)
            .await
            .expect("the workspace should bind to its exact repository worktree");

        let exact_worktree_id = project.read_with(&cx, |project, cx| {
            project
                .visible_worktrees(cx)
                .find(|worktree| worktree.read(cx).abs_path().as_ref() == Path::new(ROOT))
                .map(|worktree| worktree.read(cx).id())
        });
        assert!(exact_worktree_id.is_some());
        assert_eq!(
            cx.update(|_, cx| {
                cx.global::<BoundWorkspaces>()
                    .0
                    .get(&workspace.entity_id())
                    .and_then(|binding| binding.worktree_id)
            }),
            exact_worktree_id
        );
    }

    #[gpui::test]
    async fn test_the_binding_survives_two_workspaces_of_one_name(cx: &mut TestAppContext) {
        // An un-renamed workspace is named for its project, so every workspace
        // of one project carries the same name and only the binding can say
        // which of them a window is showing.
        let (workspace, first, mut cx) = test_window(cx).await;
        let sibling = AdeWorkspace::new(first.name.clone(), "zed", ROOT);
        assert_eq!(first.name, sibling.name);

        bind(&workspace, &first, &mut cx)
            .await
            .expect("the first workspace should bind");
        assert_eq!(
            cx.update(|_, cx| bound_workspace(workspace.entity_id(), cx).cloned()),
            Some(first.id.clone())
        );

        bind(&workspace, &sibling, &mut cx)
            .await
            .expect("the sibling workspace should bind");
        assert_eq!(
            cx.update(|_, cx| bound_workspace(workspace.entity_id(), cx).cloned()),
            Some(sibling.id.clone())
        );
    }

    #[gpui::test]
    async fn test_a_binding_only_targets_its_exact_worktree_and_host(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        bind(&workspace, &ade_workspace, &mut cx)
            .await
            .expect("the workspace should bind");

        assert_eq!(
            cx.update(|_, cx| {
                bound_workspace_for_worktree(workspace.entity_id(), Path::new(ROOT), None, cx)
                    .cloned()
            }),
            Some(ade_workspace.id.clone())
        );
        assert!(
            cx.update(|_, cx| {
                bound_workspace_for_worktree(
                    workspace.entity_id(),
                    Path::new("/repos/sibling"),
                    None,
                    cx,
                )
                .is_none()
            }),
            "another root in the same Zed workspace must not inherit the destructive target"
        );
        assert!(
            cx.update(|_, cx| {
                bound_workspace_for_worktree(
                    workspace.entity_id(),
                    Path::new(ROOT),
                    Some("other@build-box"),
                    cx,
                )
                .is_none()
            }),
            "the same path on another destination must not inherit the destructive target"
        );
    }

    #[gpui::test]
    async fn test_a_bound_worktree_move_updates_the_registry_and_binding(cx: &mut TestAppContext) {
        let (workspace, mut ade_workspace, mut cx) = test_window(cx).await;
        let registry = crate::AdeWorkspaceRegistry::open_test_db("test_bound_worktree_move").await;
        ade_workspace.project_identity = Some(ROOT.to_owned());
        registry
            .create_workspace(ade_workspace.clone())
            .await
            .unwrap();
        let lifecycle = Arc::new(crate::WorkspaceLifecycleService::with_backend(
            registry.clone(),
            Arc::new(RegistryOnlyBackend),
        ));
        cx.update(|_, cx| cx.set_global(crate::GlobalLifecycleService(lifecycle)));
        bind(&workspace, &ade_workspace, &mut cx)
            .await
            .expect("the workspace should bind before its worktree moves");

        let project = workspace.read_with(&cx, |workspace, _| workspace.project().clone());
        let (worktree_id, old_worktree_paths) = project.read_with(&cx, |project, cx| {
            let worktree_id = project
                .visible_worktrees(cx)
                .next()
                .expect("the project should have a visible worktree")
                .read(cx)
                .id();
            (worktree_id, project.worktree_paths(cx))
        });
        project.update(&mut cx, |project, cx| {
            assert!(project.update_worktree_abs_path(worktree_id, Path::new(MOVED_ROOT), cx));
            cx.emit(project::Event::WorktreePathsChanged { old_worktree_paths });
        });
        let (expected_project_id, expected_project_identity) =
            project.read_with(&cx, |project, cx| {
                let key = project.project_group_key(cx);
                (
                    key.display_name(&Default::default()).to_string(),
                    key.path_list().serialize().paths,
                )
            });
        cx.run_until_parked();

        let stored = registry
            .get_workspace(ade_workspace.id.clone())
            .unwrap()
            .expect("the bound workspace should stay recorded");
        assert_eq!(stored.repository_path, Path::new(MOVED_ROOT));
        assert_eq!(stored.project_id, expected_project_id);
        assert_eq!(
            stored.project_identity.as_deref(),
            Some(expected_project_identity.as_str())
        );
        assert_eq!(stored.project_scope_rev, 1);
        assert_eq!(
            cx.update(|_, cx| {
                bound_workspace_for_worktree(workspace.entity_id(), Path::new(MOVED_ROOT), None, cx)
                    .cloned()
            }),
            Some(ade_workspace.id.clone())
        );
        assert!(cx.update(|_, cx| {
            bound_workspace_for_worktree(workspace.entity_id(), Path::new(ROOT), None, cx).is_none()
        }));
    }

    #[gpui::test]
    async fn test_an_unrelated_remote_workspace_cannot_rebind_the_window(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        bind(&workspace, &ade_workspace, &mut cx)
            .await
            .expect("the matching workspace should bind");

        let mut unrelated = AdeWorkspace::new("Seedance", "viral-studio", "/repos/seedance");
        unrelated.remote_host = Some("user@build-box".into());
        let error = bind(&workspace, &unrelated, &mut cx)
            .await
            .expect_err("a row from another project must not take over this window");
        assert!(format!("{error:#}").contains("but this window is scoped to"));
        assert_eq!(cx.window_title().as_deref(), Some("Vector DB spike"));
        assert_eq!(
            cx.update(|_, cx| bound_workspace(workspace.entity_id(), cx).cloned()),
            Some(ade_workspace.id),
            "the failed cross-project open must preserve the existing binding"
        );
    }

    #[test]
    fn test_a_local_workspace_contributes_its_repository_path() {
        let workspace = AdeWorkspace::new("main", "zed", "/repos/zed");
        assert_eq!(worktree_path(&workspace), Some(Path::new("/repos/zed")));
    }

    #[test]
    fn test_a_remote_workspace_contributes_nothing_local() {
        let mut workspace = AdeWorkspace::new("main", "zed", "/repos/zed");
        workspace.remote_host = Some("build-box".into());
        workspace.remote_workspace_path = Some(PathBuf::from("/repos/zed"));
        // The path exists, and may even exist locally — that is exactly the
        // case that must not be opened.
        assert_eq!(worktree_path(&workspace), None);
    }
}
