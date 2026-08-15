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
use gpui::{App, AsyncWindowContext, EntityId, Global, SharedString, WeakEntity};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
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
/// Idempotent by construction: [`project::Project::find_or_create_worktree`]
/// returns the existing worktree when the path is already covered, so
/// re-selecting a workspace costs one lookup and changes nothing.
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

    let find_or_create = zed_workspace.update(cx, |zed_workspace, cx| {
        zed_workspace.project().update(cx, |project, cx| {
            project.find_or_create_worktree(path, true, cx)
        })
    });

    if let Some(find_or_create) = find_or_create.log_err() {
        find_or_create.await.log_err();
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
}

#[derive(Default)]
struct BoundWorkspaces(HashMap<EntityId, BoundWorkspace>);

impl Global for BoundWorkspaces {}

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
    let binding = BoundWorkspace {
        id: ade_workspace.id.clone(),
        repository_path: ade_workspace.repository_path.clone(),
        remote_host: ade_workspace.remote_host.clone(),
    };
    cx.update(|_, cx| {
        let entity_id = zed_workspace.entity_id();
        // One release observer per window, registered with its first binding;
        // re-opening only replaces the id that binding points at.
        if cx
            .default_global::<BoundWorkspaces>()
            .0
            .insert(entity_id, binding)
            .is_none()
        {
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
    use std::path::PathBuf;

    const ROOT: &str = "/repos/zed";

    /// A window over a one-file project, and the ADE workspace it stands for.
    async fn test_window(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, AdeWorkspace, VisualTestContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(ROOT, serde_json::json!({ "a.rs": "fn a() {}\n" }))
            .await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        let project = project::Project::test(fs, [ROOT.as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        (
            workspace,
            AdeWorkspace::new("Vector DB spike", "zed", ROOT),
            cx.clone(),
        )
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
