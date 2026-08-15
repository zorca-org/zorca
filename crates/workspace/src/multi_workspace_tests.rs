use std::{
    cell::Cell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use super::*;
use crate::item::test::TestItem;
use agent_settings::AgentSettings;
use client::{Client, UserStore, proto};
use clock::FakeSystemClock;
use fs::{FakeFs, Fs};
use gpui::{TestAppContext, VisualTestContext};
use http_client::FakeHttpClient;
use language::LanguageRegistry;
use node_runtime::NodeRuntime;
use project::DisableAiSettings;
use remote::{RemoteClient, RemoteConnectionOptions, SshConnectionOptions};
use serde_json::json;
use settings::{Settings, SettingsStore};
use util::path;

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        DisableAiSettings::register(cx);
    });
}

async fn test_remote_project(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (Entity<Project>, RemoteConnectionOptions) {
    cx.update(|cx| release_channel::init("0.0.0".parse().expect("valid test version"), cx));
    server_cx.update(|cx| release_channel::init("0.0.0".parse().expect("valid test version"), cx));
    let (host, server_session, connect_guard) = RemoteClient::fake_server(cx, server_cx);
    let ping_handler = server_cx.new(|_| ());
    server_session.add_request_handler::<rpc::proto::Ping, _, _, _>(
        ping_handler.downgrade(),
        |_entity, _envelope, _cx| async { Ok(rpc::proto::Ack {}) },
    );
    drop(connect_guard);
    let remote_client = RemoteClient::connect_mock(host.clone(), cx).await;
    let client = cx.update(|cx| {
        Client::new(
            Arc::new(FakeSystemClock::new()),
            FakeHttpClient::with_404_response(),
            cx,
        )
    });
    let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
    let languages = Arc::new(LanguageRegistry::test(cx.executor()));
    let fs = FakeFs::new(cx.executor());
    cx.update(|cx| Project::init(&client, cx));
    let project = cx.update(|cx| {
        Project::remote(
            remote_client,
            client,
            NodeRuntime::unavailable(),
            user_store,
            languages,
            fs,
            false,
            cx,
        )
    });
    (project, host)
}

#[gpui::test]
async fn test_restored_remote_groups_ignore_runtime_connection_fields(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));

    let persisted_host = RemoteConnectionOptions::Ssh(SshConnectionOptions {
        host: "example.com".into(),
        username: Some("user".to_string()),
        nickname: Some("persisted nickname".to_string()),
        ..Default::default()
    });
    let runtime_host = RemoteConnectionOptions::Ssh(SshConnectionOptions {
        host: "example.com".into(),
        username: Some("user".to_string()),
        password: Some("runtime password".to_string()),
        args: Some(vec!["-v".to_string()]),
        nickname: Some("runtime nickname".to_string()),
        ..Default::default()
    });
    let persisted_key = ProjectGroupKey::new(
        Some(persisted_host),
        PathList::new(&[PathBuf::from("/repo")]),
    );
    let runtime_key =
        ProjectGroupKey::new(Some(runtime_host), PathList::new(&[PathBuf::from("/repo")]));

    multi_workspace.update(cx, |multi_workspace, cx| {
        multi_workspace.restore_project_groups(
            vec![
                SerializedProjectGroupState {
                    key: persisted_key.clone(),
                    expanded: false,
                },
                SerializedProjectGroupState {
                    key: runtime_key.clone(),
                    expanded: true,
                },
            ],
            cx,
        );
    });

    multi_workspace.read_with(cx, |multi_workspace, _cx| {
        let groups = multi_workspace.project_group_keys();
        assert_eq!(groups.len(), 1);
        assert!(groups[0].matches(&persisted_key));
    });
}

#[gpui::test]
async fn test_restored_active_workspace_uses_persisted_project_identity_before_discovery(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    project.update(cx, |project, cx| {
        project.add_test_remote_worktree("/repo/.worktrees/feature", cx)
    });
    cx.run_until_parked();
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let identity_key = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/repo")]));

    multi_workspace.update(cx, |multi_workspace, cx| {
        multi_workspace.restore_active_workspace_project_group(identity_key.clone(), true, cx);
    });

    multi_workspace.read_with(cx, |multi_workspace, cx| {
        assert!(
            multi_workspace
                .workspace()
                .read(cx)
                .project_group_key(cx)
                .matches(&identity_key)
        );
        assert_eq!(
            multi_workspace.project_group_keys(),
            vec![identity_key.clone()]
        );
        assert_eq!(
            multi_workspace
                .workspaces_for_project_group(&identity_key, cx)
                .unwrap(),
            vec![multi_workspace.workspace().clone()]
        );
        multi_workspace
            .assert_project_group_key_integrity(cx)
            .unwrap();
    });
}

#[gpui::test]
async fn test_project_group_follows_workspace_root_move(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/old-project", json!({ "file.txt": "" }))
        .await;
    fs.insert_tree("/moved-project", json!({ "file.txt": "" }))
        .await;
    let project = Project::test(fs, ["/old-project".as_ref()], cx).await;
    let worktree_id = project.read_with(cx, |project, cx| {
        project.visible_worktrees(cx).next().unwrap().read(cx).id()
    });
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    multi_workspace.update(cx, |multi_workspace, cx| {
        multi_workspace.restore_active_workspace_project_group(
            ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/old-project")])),
            true,
            cx,
        );
    });

    let paths_changed = Rc::new(Cell::new(false));
    project.update(cx, |_, cx| {
        let paths_changed = paths_changed.clone();
        cx.subscribe(&project, move |_, _, event: &project::Event, _| {
            if matches!(event, project::Event::WorktreePathsChanged { .. }) {
                paths_changed.set(true);
            }
        })
        .detach();
    });

    project.update(cx, |project, cx| {
        assert!(project.update_worktree_abs_path(worktree_id, Path::new("/moved-project"), cx));
    });

    assert!(paths_changed.get());

    let moved_key = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/moved-project")]));
    multi_workspace.read_with(cx, |multi_workspace, cx| {
        assert_eq!(
            multi_workspace.project_group_keys(),
            vec![moved_key.clone()]
        );
        assert_eq!(
            multi_workspace
                .workspaces_for_project_group(&moved_key, cx)
                .unwrap(),
            vec![multi_workspace.workspace().clone()]
        );
        multi_workspace
            .assert_project_group_key_integrity(cx)
            .unwrap();
    });
}

#[gpui::test]
async fn test_git_discovery_replaces_a_stale_restored_project_identity(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let remote_worktree = project.update(cx, |project, cx| {
        project.add_test_remote_worktree("/linked", cx)
    });
    cx.run_until_parked();

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let stale_key = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/stale-main")]));
    multi_workspace.update(cx, |multi_workspace, cx| {
        multi_workspace.restore_active_workspace_project_group(stale_key, false, cx);
    });

    let worktree_id = remote_worktree.read_with(cx, |worktree, _cx| worktree.id().to_proto());
    remote_worktree.update(cx, |worktree, _cx| {
        worktree
            .as_remote()
            .unwrap()
            .update_from_remote(proto::UpdateWorktree {
                project_id: 0,
                worktree_id,
                abs_path: "/linked".to_string(),
                root_name: "linked".to_string(),
                updated_entries: Vec::new(),
                removed_entries: Vec::new(),
                scan_id: 1,
                is_last_update: true,
                updated_repositories: Vec::new(),
                removed_repositories: Vec::new(),
                root_repo_common_dir: Some("/actual-main/.git".to_string()),
                root_repo_is_linked_worktree: true,
            });
    });
    cx.run_until_parked();

    let actual_key = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/actual-main")]));
    multi_workspace.read_with(cx, |multi_workspace, cx| {
        assert_eq!(
            multi_workspace.project_group_keys(),
            vec![actual_key.clone()]
        );
        assert_eq!(
            multi_workspace.workspace().read(cx).project_group_key(cx),
            actual_key
        );
        multi_workspace
            .assert_project_group_key_integrity(cx)
            .unwrap();
    });
}

#[gpui::test]
async fn test_stale_restored_identity_cannot_replace_already_discovered_git_identity(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    project.update(cx, |project, cx| {
        project.add_test_remote_worktree_with_repository("/linked", Some("/actual-main/.git"), cx)
    });
    cx.run_until_parked();

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let stale_key = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/stale-main")]));
    multi_workspace.update(cx, |multi_workspace, cx| {
        multi_workspace.restore_active_workspace_project_group(stale_key, false, cx);
    });

    let actual_key = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/actual-main")]));
    multi_workspace.read_with(cx, |multi_workspace, cx| {
        assert_eq!(
            multi_workspace.project_group_keys(),
            vec![actual_key.clone()]
        );
        assert_eq!(
            multi_workspace.workspace().read(cx).project_group_key(cx),
            actual_key
        );
        multi_workspace
            .assert_project_group_key_integrity(cx)
            .unwrap();
    });
}

#[gpui::test]
async fn test_known_main_worktree_metadata_rejects_stale_restored_paths(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    project.update(cx, |project, cx| {
        project.add_test_remote_worktree_with_repository(
            "/actual-main",
            Some("/actual-main/.git"),
            cx,
        )
    });
    cx.run_until_parked();

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let stale_key = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/stale-main")]));
    multi_workspace.update(cx, |multi_workspace, cx| {
        multi_workspace.restore_active_workspace_project_group(stale_key, false, cx);
    });

    let actual_key = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/actual-main")]));
    multi_workspace.read_with(cx, |multi_workspace, cx| {
        assert_eq!(
            multi_workspace.project_group_keys(),
            vec![actual_key.clone()]
        );
        assert_eq!(
            multi_workspace.workspace().read(cx).project_group_key(cx),
            actual_key
        );
    });
}

#[gpui::test]
async fn test_completed_non_git_worktree_rejects_stale_restored_paths(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let remote_worktree = project.update(cx, |project, cx| {
        project.add_test_remote_worktree("/actual-main", cx)
    });
    cx.run_until_parked();

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let stale_key = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/stale-main")]));
    multi_workspace.update(cx, |multi_workspace, cx| {
        multi_workspace.restore_active_workspace_project_group(stale_key, false, cx);
    });

    let worktree_id = remote_worktree.read_with(cx, |worktree, _cx| worktree.id().to_proto());
    remote_worktree.update(cx, |worktree, _cx| {
        worktree
            .as_remote()
            .expect("test worktree should be remote")
            .update_from_remote(proto::UpdateWorktree {
                project_id: 0,
                worktree_id,
                abs_path: "/actual-main".to_string(),
                root_name: "actual-main".to_string(),
                updated_entries: Vec::new(),
                removed_entries: Vec::new(),
                scan_id: 1,
                is_last_update: true,
                updated_repositories: Vec::new(),
                removed_repositories: Vec::new(),
                root_repo_common_dir: None,
                root_repo_is_linked_worktree: false,
            });
    });
    cx.run_until_parked();

    let actual_key = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/actual-main")]));
    multi_workspace.read_with(cx, |multi_workspace, cx| {
        assert_eq!(
            multi_workspace.project_group_keys(),
            vec![actual_key.clone()]
        );
        assert_eq!(
            multi_workspace.workspace().read(cx).project_group_key(cx),
            actual_key
        );
    });
}

#[gpui::test]
async fn test_stale_restored_host_cannot_replace_a_live_workspace_host(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (project, live_host) = test_remote_project(cx, server_cx).await;
    project.update(cx, |project, cx| {
        project.add_test_remote_worktree("/repo", cx)
    });
    cx.run_until_parked();

    let stale_host = RemoteConnectionOptions::Mock(remote::MockConnectionOptions { id: u64::MAX });
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    multi_workspace.update(cx, |multi_workspace, cx| {
        multi_workspace.restore_active_workspace_project_group(
            ProjectGroupKey::new(Some(stale_host), PathList::new(&[PathBuf::from("/repo")])),
            false,
            cx,
        );
    });

    let expected = ProjectGroupKey::new(Some(live_host), PathList::new(&[PathBuf::from("/repo")]));
    multi_workspace.read_with(cx, |multi_workspace, cx| {
        assert_eq!(multi_workspace.project_group_keys(), vec![expected.clone()]);
        assert!(
            multi_workspace
                .workspace()
                .read(cx)
                .project_group_key(cx)
                .matches(&expected)
        );
    });
}

#[gpui::test]
async fn test_open_mode_add_reuses_a_racing_workspace_without_activating_it(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/projects",
        json!({
            "source": { "source.txt": "" },
            "target": { "target.txt": "" },
        }),
    )
    .await;
    cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
    let source_project = Project::test(fs.clone(), ["/projects/source".as_ref()], cx).await;
    let target_project = Project::test(fs, ["/projects/target".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(source_project, window, cx));
    let source_workspace =
        multi_workspace.read_with(cx, |workspace, _cx| workspace.workspace().clone());
    multi_workspace.update(cx, |workspace, cx| workspace.retain_active_workspace(cx));

    let open_task = multi_workspace.update_in(cx, |workspace, window, cx| {
        workspace.find_or_create_local_workspace(
            PathList::new(&[PathBuf::from("/projects/target")]),
            None,
            &[],
            None,
            OpenMode::Add,
            window,
            cx,
        )
    });
    let target_workspace = multi_workspace.update_in(cx, |workspace, window, cx| {
        workspace.test_add_workspace(target_project, window, cx)
    });
    multi_workspace.update_in(cx, |workspace, window, cx| {
        workspace.activate(source_workspace.clone(), None, window, cx);
    });

    let opened_workspace = open_task
        .await
        .expect("the workspace added during the open should be reused");
    assert_eq!(opened_workspace, target_workspace);
    assert_eq!(
        multi_workspace.read_with(cx, |workspace, _cx| workspace.workspace().clone()),
        source_workspace,
        "OpenMode::Add must not activate a workspace discovered while opening"
    );
}

#[gpui::test]
async fn test_remote_open_mode_add_returns_the_exact_background_workspace(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (project, host) = test_remote_project(cx, server_cx).await;
    project.update(cx, |project, cx| {
        project.add_test_remote_worktree("/remote/source", cx);
        project.add_test_remote_worktree("/remote/created", cx);
    });
    cx.run_until_parked();

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let source_workspace =
        multi_workspace.read_with(cx, |workspace, _cx| workspace.workspace().clone());
    let source_focus = cx.update(|window, cx| {
        window
            .focused(cx)
            .expect("the source workspace should start focused")
    });
    let app_state = source_workspace.read_with(cx, |workspace, _cx| workspace.app_state().clone());
    let window_handle = cx.update(|window, _cx| {
        window
            .window_handle()
            .downcast::<MultiWorkspace>()
            .expect("test window should contain a MultiWorkspace")
    });
    let open_task = cx.update(|_window, cx| {
        cx.spawn(async move |cx| {
            crate::open_remote_project_with_existing_connection_in_mode(
                host,
                project,
                vec![PathBuf::from("/remote/created")],
                app_state,
                window_handle,
                None,
                None,
                OpenMode::Add,
                cx,
            )
            .await
        })
    });

    let (opened_workspace, _) = open_task
        .await
        .expect("the remote workspace should open in the background");
    assert_ne!(opened_workspace, source_workspace);
    assert_eq!(
        multi_workspace.read_with(cx, |workspace, _cx| workspace.workspace().clone()),
        source_workspace,
        "opening a remote background workspace must not change the active workspace"
    );
    cx.update(|window, _cx| {
        assert!(
            source_focus.is_focused(window),
            "opening a remote background workspace must preserve focus"
        );
    });
    assert!(
        opened_workspace
            .read_with(cx, |workspace, cx| workspace.root_paths(cx))
            .iter()
            .any(|path| path.as_ref() == Path::new("/remote/created")),
        "the returned workspace must contain the requested remote worktree"
    );
}

#[gpui::test]
async fn test_remote_open_reuses_a_workspace_added_while_connecting(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    let (remote_project, host) = test_remote_project(cx, server_cx).await;
    let open_client = remote_project.read_with(cx, |project, _cx| {
        project
            .remote_client()
            .expect("the remote test project should have a client")
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/",
        json!({
            "local": { "source.txt": "" },
            "remote": { "created": { "target.txt": "" } },
        }),
    )
    .await;
    cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
    let source_project = Project::test(fs.clone(), ["/local".as_ref()], cx).await;
    let target_project = Project::test(fs, ["/remote/created".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(source_project, window, cx));
    let source_workspace =
        multi_workspace.read_with(cx, |workspace, _cx| workspace.workspace().clone());
    multi_workspace.update(cx, |workspace, cx| workspace.retain_active_workspace(cx));

    let open_task = multi_workspace.update_in(cx, |workspace, window, cx| {
        workspace.find_or_create_workspace(
            PathList::new(&[PathBuf::from("/remote/created")]),
            Some(host.clone()),
            None,
            move |_options, _window, _cx| Task::ready(Ok(Some(open_client))),
            &[],
            None,
            OpenMode::Add,
            window,
            cx,
        )
    });
    let target_workspace = multi_workspace.update_in(cx, |workspace, window, cx| {
        let target_workspace = workspace.test_add_workspace(target_project, window, cx);
        target_workspace.update(cx, |target_workspace, _cx| {
            target_workspace.test_set_project_group_key_hint(ProjectGroupKey::new(
                Some(host.clone()),
                PathList::new(&[PathBuf::from("/remote/created")]),
            ));
        });
        target_workspace
    });
    multi_workspace.update_in(cx, |workspace, window, cx| {
        workspace.activate(source_workspace.clone(), None, window, cx);
    });

    let opened_workspace = open_task
        .await
        .expect("the workspace added during remote connection should be reused");
    assert_eq!(opened_workspace, target_workspace);
    assert_eq!(
        multi_workspace.read_with(cx, |workspace, _cx| workspace.workspace().clone()),
        source_workspace,
        "reusing a remote background workspace must not activate it"
    );
    assert_eq!(
        multi_workspace.read_with(cx, |workspace, _cx| workspace.workspaces().count()),
        2,
        "a concurrent remote open must not duplicate the workspace"
    );
}

#[gpui::test]
async fn test_sidebar_stays_enabled_when_disable_ai_is_enabled(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));

    multi_workspace.read_with(cx, |mw, cx| {
        assert!(mw.multi_workspace_enabled(cx));
    });

    multi_workspace.update_in(cx, |mw, _window, cx| {
        mw.open_sidebar(cx);
        assert!(mw.sidebar_open());
    });

    cx.update(|_window, cx| {
        DisableAiSettings::override_global(DisableAiSettings { disable_ai: true }, cx);
    });
    cx.run_until_parked();

    multi_workspace.read_with(cx, |mw, cx| {
        assert!(
            mw.sidebar_open(),
            "ZOrca's sidebar navigates worktrees and terminals, so disable_ai must not close it"
        );
        assert!(
            mw.multi_workspace_enabled(cx),
            "Multi-workspace must stay enabled when disable_ai is true"
        );
    });

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.toggle_sidebar(window, cx);
    });
    multi_workspace.read_with(cx, |mw, _cx| {
        assert!(
            !mw.sidebar_open(),
            "Sidebar should remain closed when toggled with disable_ai true"
        );
    });

    cx.update(|_window, cx| {
        DisableAiSettings::override_global(DisableAiSettings { disable_ai: false }, cx);
    });
    cx.run_until_parked();

    multi_workspace.read_with(cx, |mw, cx| {
        assert!(
            mw.multi_workspace_enabled(cx),
            "Multi-workspace should be enabled after re-enabling AI"
        );
        assert!(
            !mw.sidebar_open(),
            "Sidebar should still be closed after re-enabling AI (not auto-opened)"
        );
    });

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.toggle_sidebar(window, cx);
    });
    multi_workspace.read_with(cx, |mw, _cx| {
        assert!(
            mw.sidebar_open(),
            "Sidebar should open when toggled after re-enabling AI"
        );
    });
}

#[gpui::test]
async fn test_multi_workspace_collapses_when_agent_is_disabled(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    fs.insert_tree("/root_b", json!({ "file.txt": "" })).await;
    let project_a = Project::test(fs.clone(), ["/root_a".as_ref()], cx).await;
    let project_b = Project::test(fs, ["/root_b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(project_b, window, cx);
    });
    cx.run_until_parked();

    multi_workspace.read_with(cx, |multi_workspace, cx| {
        assert!(multi_workspace.multi_workspace_enabled(cx));
        assert_eq!(multi_workspace.workspaces().count(), 2);
    });

    cx.update(|_window, cx| {
        let mut settings = AgentSettings::get_global(cx).clone();
        settings.enabled = false;
        AgentSettings::override_global(settings, cx);
    });
    cx.run_until_parked();

    multi_workspace.read_with(cx, |multi_workspace, cx| {
        assert!(!multi_workspace.multi_workspace_enabled(cx));
        assert!(!multi_workspace.sidebar_open());
        assert_eq!(multi_workspace.workspaces().count(), 1);
        assert!(multi_workspace.project_group_keys().is_empty());
    });
}

#[gpui::test]
async fn test_project_group_keys_initial(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    let project = Project::test(fs, ["/root_a".as_ref()], cx).await;

    let expected_key = project.read_with(cx, |project, cx| project.project_group_key(cx));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));

    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
    });

    multi_workspace.read_with(cx, |mw, _cx| {
        let keys: Vec<ProjectGroupKey> = mw.project_group_keys();
        assert_eq!(keys.len(), 1, "should have exactly one key on creation");
        assert_eq!(keys[0], expected_key);
    });
}

#[gpui::test]
async fn test_project_group_keys_add_workspace(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    fs.insert_tree("/root_b", json!({ "file.txt": "" })).await;
    let project_a = Project::test(fs.clone(), ["/root_a".as_ref()], cx).await;
    let project_b = Project::test(fs.clone(), ["/root_b".as_ref()], cx).await;

    let key_a = project_a.read_with(cx, |p, cx| p.project_group_key(cx));
    let key_b = project_b.read_with(cx, |p, cx| p.project_group_key(cx));
    assert_ne!(
        key_a, key_b,
        "different roots should produce different keys"
    );

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
    });

    multi_workspace.read_with(cx, |mw, _cx| {
        assert_eq!(mw.project_group_keys().len(), 1);
    });

    // Adding a workspace with a different project root adds a new key.
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx);
    });

    multi_workspace.read_with(cx, |mw, _cx| {
        let keys: Vec<ProjectGroupKey> = mw.project_group_keys();
        assert_eq!(
            keys.len(),
            2,
            "should have two keys after adding a second workspace"
        );
        assert_eq!(keys[0], key_b);
        assert_eq!(keys[1], key_a);
    });
}

#[gpui::test]
async fn test_open_new_window_does_not_open_sidebar_on_existing_window(cx: &mut TestAppContext) {
    init_test(cx);

    let app_state = cx.update(AppState::test);
    let fs = app_state.fs.as_fake();
    fs.insert_tree(path!("/project_a"), json!({ "file.txt": "" }))
        .await;
    fs.insert_tree(path!("/project_b"), json!({ "file.txt": "" }))
        .await;

    let project = Project::test(app_state.fs.clone(), [path!("/project_a").as_ref()], cx).await;

    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

    // ZOrca opens the sidebar by default, so close it first: the invariant under test
    // is that opening a project elsewhere does not force it back open.
    window
        .update(cx, |mw, window, cx| mw.close_sidebar(window, cx))
        .unwrap();

    cx.update(|cx| {
        open_paths(
            &[PathBuf::from(path!("/project_b"))],
            app_state,
            OpenOptions {
                open_mode: OpenMode::NewWindow,
                ..OpenOptions::default()
            },
            cx,
        )
    })
    .await
    .unwrap();

    window
        .read_with(cx, |mw, _cx| {
            assert!(
                !mw.sidebar_open(),
                "opening a project in a new window must not open the sidebar on the original window",
            );
        })
        .unwrap();
}

#[gpui::test]
async fn test_open_directory_in_empty_workspace_does_not_open_sidebar(cx: &mut TestAppContext) {
    init_test(cx);

    let app_state = cx.update(AppState::test);
    let fs = app_state.fs.as_fake();
    fs.insert_tree(path!("/project"), json!({ "file.txt": "" }))
        .await;

    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| {
        let mw = MultiWorkspace::test_new(project, window, cx);
        // Simulate a blank project that has an untitled editor tab,
        // so that workspace_windows_for_location finds this window.
        mw.workspace().update(cx, |workspace, cx| {
            workspace.active_pane().update(cx, |pane, cx| {
                let item = cx.new(|cx| item::test::TestItem::new(cx));
                pane.add_item(Box::new(item), false, false, None, window, cx);
            });
        });
        mw
    });

    // ZOrca opens the sidebar by default, so close it first: the invariant under test
    // is that opening a project elsewhere does not force it back open.
    window
        .update(cx, |mw, window, cx| mw.close_sidebar(window, cx))
        .unwrap();

    // Simulate what open_workspace_for_paths does for an empty workspace:
    // it downgrades OpenMode::NewWindow to Activate and sets requesting_window.
    cx.update(|cx| {
        open_paths(
            &[PathBuf::from(path!("/project"))],
            app_state,
            OpenOptions {
                requesting_window: Some(window),
                open_mode: OpenMode::Activate,
                ..OpenOptions::default()
            },
            cx,
        )
    })
    .await
    .unwrap();

    window
        .read_with(cx, |mw, _cx| {
            assert!(
                !mw.sidebar_open(),
                "opening a directory in a blank project via the file picker must not open the sidebar",
            );
        })
        .unwrap();
}

#[gpui::test]
async fn test_project_group_keys_duplicate_not_added(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    let project_a = Project::test(fs.clone(), ["/root_a".as_ref()], cx).await;
    // A second project entity pointing at the same path produces the same key.
    let project_a2 = Project::test(fs.clone(), ["/root_a".as_ref()], cx).await;

    let key_a = project_a.read_with(cx, |p, cx| p.project_group_key(cx));
    let key_a2 = project_a2.read_with(cx, |p, cx| p.project_group_key(cx));
    assert_eq!(key_a, key_a2, "same root path should produce the same key");

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
    });

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_a2, window, cx);
    });

    multi_workspace.read_with(cx, |mw, _cx| {
        let keys: Vec<ProjectGroupKey> = mw.project_group_keys();
        assert_eq!(
            keys.len(),
            1,
            "duplicate key should not be added when a workspace with the same root is inserted"
        );
    });
}

#[gpui::test]
async fn test_adding_worktree_updates_project_group_key(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    fs.insert_tree("/root_b", json!({ "other.txt": "" })).await;
    let project = Project::test(fs.clone(), ["/root_a".as_ref()], cx).await;

    let initial_key = project.read_with(cx, |p, cx| p.project_group_key(cx));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));

    // Open sidebar to retain the workspace and create the initial group.
    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
    });
    cx.run_until_parked();

    multi_workspace.read_with(cx, |mw, _cx| {
        let keys = mw.project_group_keys();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0], initial_key);
    });

    // Add a second worktree to the project. This triggers WorktreeAdded →
    // handle_workspace_key_change, which should update the group key.
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/root_b", true, cx)
        })
        .await
        .expect("adding worktree should succeed");
    cx.run_until_parked();

    let updated_key = project.read_with(cx, |p, cx| p.project_group_key(cx));
    assert_ne!(
        initial_key, updated_key,
        "adding a worktree should change the project group key"
    );

    multi_workspace.read_with(cx, |mw, _cx| {
        let keys = mw.project_group_keys();
        assert!(
            keys.contains(&updated_key),
            "should contain the updated key; got {keys:?}"
        );
    });
}

#[gpui::test]
async fn test_find_or_create_local_workspace_reuses_active_workspace_when_sidebar_closed(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    let project = Project::test(fs, ["/root_a".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));

    let active_workspace = multi_workspace.read_with(cx, |mw, cx| {
        assert!(
            mw.project_groups(cx).is_empty(),
            "sidebar-closed setup should start with no retained project groups"
        );
        mw.workspace().clone()
    });
    let active_workspace_id = active_workspace.entity_id();

    let workspace = multi_workspace
        .update_in(cx, |mw, window, cx| {
            mw.find_or_create_local_workspace(
                PathList::new(&[PathBuf::from("/root_a")]),
                None,
                &[],
                None,
                OpenMode::Activate,
                window,
                cx,
            )
        })
        .await
        .expect("reopening the same local workspace should succeed");

    assert_eq!(
        workspace.entity_id(),
        active_workspace_id,
        "should reuse the current active workspace when the sidebar is closed"
    );

    multi_workspace.read_with(cx, |mw, _cx| {
        assert_eq!(
            mw.workspace().entity_id(),
            active_workspace_id,
            "active workspace should remain unchanged after reopening the same path"
        );
        assert_eq!(
            mw.workspaces().count(),
            1,
            "reusing the active workspace should not create a second open workspace"
        );
    });
}

#[gpui::test]
async fn test_find_or_create_workspace_uses_project_group_key_when_paths_are_missing(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
    let project = Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let project_group_key = project.read_with(cx, |project, cx| project.project_group_key(cx));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));

    let main_workspace = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());
    let main_workspace_id = main_workspace.entity_id();

    let workspace = multi_workspace
        .update_in(cx, |mw, window, cx| {
            mw.find_or_create_workspace(
                PathList::new(&[PathBuf::from("/wt-feature-a")]),
                None,
                Some(project_group_key.clone()),
                |_options, _window, _cx| Task::ready(Ok(None)),
                &[],
                None,
                OpenMode::Activate,
                window,
                cx,
            )
        })
        .await
        .expect("opening a missing linked-worktree path should fall back to the project group key workspace");

    assert_eq!(
        workspace.entity_id(),
        main_workspace_id,
        "missing linked-worktree paths should reuse the main worktree workspace from the project group key"
    );

    multi_workspace.read_with(cx, |mw, cx| {
        assert_eq!(
            mw.workspace().entity_id(),
            main_workspace_id,
            "the active workspace should remain the main worktree workspace"
        );
        assert_eq!(
            PathList::new(&mw.workspace().read(cx).root_paths(cx)),
            project_group_key.path_list().clone(),
            "the activated workspace should use the project group key path list rather than the missing linked-worktree path"
        );
        assert_eq!(
            mw.workspaces().count(),
            1,
            "falling back to the project group key should not create a second workspace"
        );
    });
}

#[gpui::test]
async fn test_find_or_create_workspace_rejects_missing_project_group_path(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project", json!({ ".git": {} })).await;
    cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
    let project = Project::test(fs, ["/project".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let missing_paths = PathList::new(&[PathBuf::from("/missing-worktree")]);
    let project_groups = [
        None,
        Some(ProjectGroupKey::new(None, missing_paths.clone())),
        Some(ProjectGroupKey::new(
            None,
            PathList::new(&[PathBuf::from("/missing-main-worktree")]),
        )),
    ];

    for project_group in project_groups {
        let result = multi_workspace
            .update_in(cx, |multi_workspace, window, cx| {
                multi_workspace.find_or_create_workspace(
                    missing_paths.clone(),
                    None,
                    project_group,
                    |_options, _window, _cx| Task::ready(Ok(None)),
                    &[],
                    None,
                    OpenMode::Activate,
                    window,
                    cx,
                )
            })
            .await;

        assert!(result.is_err(), "a missing project path must not be opened");
    }
    multi_workspace.read_with(cx, |multi_workspace, _cx| {
        assert_eq!(
            multi_workspace.workspaces().count(),
            1,
            "rejecting a missing path must not create a workspace"
        );
    });
}

#[gpui::test]
async fn test_remove_fallback_via_find_or_create_skips_removed_workspaces(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    let project_a = Project::test(fs.clone(), ["/root_a".as_ref()], cx).await;
    let project_b = Project::test(fs, ["/root_a".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    let workspace_a = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());
    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx)
    });
    cx.run_until_parked();

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a.clone(), None, window, cx);
    });

    let removed = multi_workspace
        .update_in(cx, |mw, window, cx| {
            let excluded = vec![workspace_a.clone()];
            mw.remove(
                excluded.clone(),
                move |this, window, cx| {
                    this.find_or_create_workspace(
                        PathList::new(&[PathBuf::from("/root_a")]),
                        None,
                        None,
                        |_options, _window, _cx| Task::ready(Ok(None)),
                        &excluded,
                        None,
                        OpenMode::Activate,
                        window,
                        cx,
                    )
                },
                window,
                cx,
            )
        })
        .await
        .expect("removing the active workspace should succeed");
    assert!(removed, "the workspace should have been removed");

    multi_workspace.read_with(cx, |mw, _cx| {
        assert_eq!(
            mw.workspace().entity_id(),
            workspace_b.entity_id(),
            "the non-excluded workspace should become active"
        );
        assert!(
            mw.workspaces()
                .all(|workspace| workspace.entity_id() != workspace_a.entity_id()),
            "the removed workspace should be gone"
        );
    });
}

#[gpui::test]
async fn test_find_or_create_local_workspace_reuses_active_workspace_after_sidebar_open(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    let project = Project::test(fs, ["/root_a".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));

    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
    });
    cx.run_until_parked();

    let active_workspace = multi_workspace.read_with(cx, |mw, cx| {
        assert_eq!(
            mw.project_groups(cx).len(),
            1,
            "opening the sidebar should retain the active workspace in a project group"
        );
        mw.workspace().clone()
    });
    let active_workspace_id = active_workspace.entity_id();

    let workspace = multi_workspace
        .update_in(cx, |mw, window, cx| {
            mw.find_or_create_local_workspace(
                PathList::new(&[PathBuf::from("/root_a")]),
                None,
                &[],
                None,
                OpenMode::Activate,
                window,
                cx,
            )
        })
        .await
        .expect("reopening the same retained local workspace should succeed");

    assert_eq!(
        workspace.entity_id(),
        active_workspace_id,
        "should reuse the retained active workspace after the sidebar is opened"
    );

    multi_workspace.read_with(cx, |mw, _cx| {
        assert_eq!(
            mw.workspaces().count(),
            1,
            "reopening the same retained workspace should not create another workspace"
        );
    });
}

#[gpui::test]
async fn test_close_workspace_prefers_already_loaded_neighboring_workspace(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file_a.txt": "" })).await;
    fs.insert_tree("/root_b", json!({ "file_b.txt": "" })).await;
    fs.insert_tree("/root_c", json!({ "file_c.txt": "" })).await;
    let project_a = Project::test(fs.clone(), ["/root_a".as_ref()], cx).await;
    let project_b = Project::test(fs.clone(), ["/root_b".as_ref()], cx).await;
    let project_b_key = project_b.read_with(cx, |project, cx| project.project_group_key(cx));
    let project_c = Project::test(fs, ["/root_c".as_ref()], cx).await;
    let project_c_key = project_c.read_with(cx, |project, cx| project.project_group_key(cx));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    multi_workspace.update(cx, |multi_workspace, cx| {
        multi_workspace.open_sidebar(cx);
    });
    cx.run_until_parked();

    let workspace_a = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });
    let workspace_b = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(project_b, window, cx)
    });

    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.activate(workspace_a.clone(), None, window, cx);
        multi_workspace.test_add_project_group(ProjectGroup {
            key: project_c_key.clone(),
            workspaces: Vec::new(),
            expanded: true,
        });
    });

    multi_workspace.read_with(cx, |multi_workspace, _cx| {
        let keys = multi_workspace.project_group_keys();
        assert_eq!(
            keys.len(),
            3,
            "expected three project groups in the test setup"
        );
        assert_eq!(keys[0], project_b_key);
        assert_eq!(
            keys[1],
            workspace_a.read_with(cx, |workspace, cx| { workspace.project_group_key(cx) })
        );
        assert_eq!(keys[2], project_c_key);
        assert_eq!(
            multi_workspace.workspace().entity_id(),
            workspace_a.entity_id(),
            "workspace A should be active before closing"
        );
    });

    let closed = multi_workspace
        .update_in(cx, |multi_workspace, window, cx| {
            multi_workspace.close_workspace(&workspace_a, window, cx)
        })
        .await
        .expect("closing the active workspace should succeed");

    assert!(
        closed,
        "close_workspace should report that it removed a workspace"
    );

    multi_workspace.read_with(cx, |multi_workspace, cx| {
        assert_eq!(
            multi_workspace.workspace().entity_id(),
            workspace_b.entity_id(),
            "closing workspace A should activate the already-loaded workspace B instead of opening group C"
        );
        assert_eq!(
            multi_workspace.workspaces().count(),
            1,
            "only workspace B should remain loaded after closing workspace A"
        );
        assert!(
            multi_workspace
                .workspaces_for_project_group(&project_c_key, cx)
                .unwrap_or_default()
                .is_empty(),
            "the unloaded neighboring group C should remain unopened"
        );
    });
}

#[gpui::test]
async fn test_switching_projects_with_sidebar_closed_retains_old_active_workspace(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file_a.txt": "" })).await;
    fs.insert_tree("/root_b", json!({ "file_b.txt": "" })).await;
    let project_a = Project::test(fs.clone(), ["/root_a".as_ref()], cx).await;
    let project_b = Project::test(fs, ["/root_b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    let workspace_a = multi_workspace.read_with(cx, |mw, cx| {
        assert!(
            mw.project_groups(cx).is_empty(),
            "sidebar-closed setup should start with no retained project groups"
        );
        mw.workspace().clone()
    });
    assert!(
        workspace_a.read_with(cx, |workspace, _cx| workspace.session_id().is_some()),
        "initial active workspace should start attached to the session"
    );

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b, window, cx)
    });
    cx.run_until_parked();

    multi_workspace.read_with(cx, |mw, cx| {
        assert_eq!(
            mw.workspace().entity_id(),
            workspace_b.entity_id(),
            "the new workspace should become active"
        );
        assert_eq!(
            mw.workspaces().count(),
            2,
            "the previous active workspace should remain open after switching with the sidebar closed"
        );
        assert_eq!(mw.project_groups(cx).len(), 2);
    });

    assert!(
        workspace_a.read_with(cx, |workspace, _cx| workspace.session_id().is_some()),
        "the previous active workspace should remain attached when switching away with the sidebar closed"
    );
}

#[gpui::test]
async fn test_activating_workspace_with_source_keeps_items_scoped(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/worktree-a", json!({ "a.txt": "" })).await;
    fs.insert_tree("/worktree-b", json!({ "b.txt": "" })).await;
    let project_a = Project::test(fs.clone(), ["/worktree-a".as_ref()], cx).await;
    let project_b = Project::test(fs, ["/worktree-b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let workspace_a = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });
    let item_a = cx.new(|cx| TestItem::new(cx).with_label("workspace-a"));
    workspace_a.update_in(cx, |workspace, window, cx| {
        workspace.add_item_to_active_pane(Box::new(item_a.clone()), None, true, window, cx);
    });

    let workspace_b = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        let workspace_b = cx.new(|cx| Workspace::test_new(project_b, window, cx));
        multi_workspace.activate(
            workspace_b.clone(),
            Some(workspace_a.downgrade()),
            window,
            cx,
        );
        workspace_b
    });

    workspace_b.read_with(cx, |workspace, cx| {
        assert_eq!(
            workspace.items(cx).count(),
            0,
            "activating a workspace with a source must not copy the source's tabs"
        );
    });

    let item_b = cx.new(|cx| TestItem::new(cx).with_label("workspace-b"));
    workspace_b.update_in(cx, |workspace, window, cx| {
        workspace.add_item_to_active_pane(Box::new(item_b.clone()), None, true, window, cx);
    });

    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.activate(workspace_a.clone(), None, window, cx);
    });
    workspace_a.read_with(cx, |workspace, cx| {
        assert_eq!(
            workspace
                .items(cx)
                .map(|item| item.item_id())
                .collect::<Vec<_>>(),
            vec![item_a.item_id()]
        );
    });

    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.activate(workspace_b.clone(), None, window, cx);
    });
    workspace_b.read_with(cx, |workspace, cx| {
        assert_eq!(
            workspace
                .items(cx)
                .map(|item| item.item_id())
                .collect::<Vec<_>>(),
            vec![item_b.item_id()]
        );
    });
}

#[gpui::test]
async fn test_remote_project_root_dir_changes_update_groups(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    fs.insert_tree("/local_b", json!({ "file.txt": "" })).await;
    let project_a = Project::test(fs.clone(), ["/root_a".as_ref()], cx).await;
    let project_b = Project::test(fs.clone(), ["/local_b".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
    });
    cx.run_until_parked();

    let workspace_b = multi_workspace.update_in(cx, |mw, window, cx| {
        let workspace = cx.new(|cx| Workspace::test_new(project_b.clone(), window, cx));
        let key = workspace.read(cx).project_group_key(cx);
        mw.activate_provisional_workspace(workspace.clone(), key, window, cx);
        workspace
    });
    cx.run_until_parked();

    multi_workspace.read_with(cx, |mw, _cx| {
        assert_eq!(
            mw.workspace().entity_id(),
            workspace_b.entity_id(),
            "registered workspace should become active"
        );
    });

    let initial_key = project_b.read_with(cx, |p, cx| p.project_group_key(cx));
    multi_workspace.read_with(cx, |mw, _cx| {
        let keys = mw.project_group_keys();
        assert!(
            keys.contains(&initial_key),
            "project groups should contain the initial key for the registered workspace"
        );
    });

    let remote_worktree = project_b.update(cx, |project, cx| {
        project.add_test_remote_worktree("/remote/project", cx)
    });
    cx.run_until_parked();

    let worktree_id = remote_worktree.read_with(cx, |wt, _| wt.id().to_proto());
    remote_worktree.update(cx, |worktree, _cx| {
        worktree
            .as_remote()
            .unwrap()
            .update_from_remote(proto::UpdateWorktree {
                project_id: 0,
                worktree_id,
                abs_path: "/remote/project".to_string(),
                root_name: "project".to_string(),
                updated_entries: vec![proto::Entry {
                    id: 1,
                    is_dir: true,
                    path: "".to_string(),
                    inode: 1,
                    mtime: Some(proto::Timestamp {
                        seconds: 0,
                        nanos: 0,
                    }),
                    is_ignored: false,
                    is_hidden: false,
                    is_external: false,
                    is_fifo: false,
                    size: None,
                    canonical_path: None,
                }],
                removed_entries: vec![],
                scan_id: 1,
                is_last_update: true,
                updated_repositories: vec![],
                removed_repositories: vec![],
                root_repo_common_dir: None,
                root_repo_is_linked_worktree: false,
            });
    });
    cx.run_until_parked();

    let updated_key = project_b.read_with(cx, |p, cx| p.project_group_key(cx));
    assert_ne!(
        initial_key, updated_key,
        "remote worktree update should change the project group key"
    );

    multi_workspace.read_with(cx, |mw, _cx| {
        let keys = mw.project_group_keys();
        assert!(
            keys.contains(&updated_key),
            "project groups should contain the updated key after remote change; got {keys:?}"
        );
        assert!(
            !keys.contains(&initial_key),
            "project groups should no longer contain the stale initial key; got {keys:?}"
        );
    });
}

#[gpui::test]
async fn test_open_project_closes_empty_workspace_but_not_non_empty_ones(cx: &mut TestAppContext) {
    init_test(cx);
    let app_state = cx.update(AppState::test);
    let fs = app_state.fs.as_fake();
    fs.insert_tree(path!("/project_a"), json!({ "file_a.txt": "" }))
        .await;
    fs.insert_tree(path!("/project_b"), json!({ "file_b.txt": "" }))
        .await;

    // Start with an empty (no-worktrees) workspace.
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    cx.run_until_parked();

    window
        .update(cx, |mw, _window, cx| mw.open_sidebar(cx))
        .unwrap();
    cx.run_until_parked();

    let empty_workspace = window
        .read_with(cx, |mw, _| mw.workspace().clone())
        .unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);

    // Add a dirty untitled item to the empty workspace.
    let dirty_item = cx.new(|cx| TestItem::new(cx).with_dirty(true));
    empty_workspace.update_in(cx, |workspace, window, cx| {
        workspace.add_item_to_active_pane(Box::new(dirty_item.clone()), None, true, window, cx);
    });

    // Opening a project while the lone empty workspace has unsaved
    // changes prompts the user.
    let open_task = window
        .update(cx, |mw, window, cx| {
            mw.open_project(
                vec![PathBuf::from(path!("/project_a"))],
                OpenMode::Activate,
                window,
                cx,
            )
        })
        .unwrap();
    cx.run_until_parked();

    // Cancelling keeps the empty workspace.
    assert!(cx.has_pending_prompt(),);
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(open_task.await.unwrap(), empty_workspace);
    window
        .read_with(cx, |mw, _cx| {
            assert_eq!(mw.workspaces().count(), 1);
            assert_eq!(mw.workspace(), &empty_workspace);
            assert_eq!(mw.project_group_keys(), vec![]);
        })
        .unwrap();

    // Discarding the unsaved changes closes the empty workspace
    // and opens the new project in its place.
    let open_task = window
        .update(cx, |mw, window, cx| {
            mw.open_project(
                vec![PathBuf::from(path!("/project_a"))],
                OpenMode::Activate,
                window,
                cx,
            )
        })
        .unwrap();
    cx.run_until_parked();

    assert!(cx.has_pending_prompt(),);
    cx.simulate_prompt_answer("Don't Save");
    cx.run_until_parked();

    let workspace_a = open_task.await.unwrap();
    assert_ne!(workspace_a, empty_workspace);

    window
        .read_with(cx, |mw, _cx| {
            assert_eq!(mw.workspaces().count(), 1);
            assert_eq!(mw.workspace(), &workspace_a);
            assert_eq!(
                mw.project_group_keys(),
                vec![ProjectGroupKey::new(
                    None,
                    PathList::new(&[path!("/project_a")])
                )]
            );
        })
        .unwrap();
    assert!(
        empty_workspace.read_with(cx, |workspace, _cx| workspace.session_id().is_none()),
        "the detached empty workspace should no longer be attached to the session",
    );

    let dirty_item = cx.new(|cx| TestItem::new(cx).with_dirty(true));
    workspace_a.update_in(cx, |workspace, window, cx| {
        workspace.add_item_to_active_pane(Box::new(dirty_item.clone()), None, true, window, cx);
    });

    // Opening another project does not close the existing project or prompt.
    let workspace_b = window
        .update(cx, |mw, window, cx| {
            mw.open_project(
                vec![PathBuf::from(path!("/project_b"))],
                OpenMode::Activate,
                window,
                cx,
            )
        })
        .unwrap()
        .await
        .unwrap();
    cx.run_until_parked();

    assert!(!cx.has_pending_prompt());
    assert_ne!(workspace_b, workspace_a);
    window
        .read_with(cx, |mw, _cx| {
            assert_eq!(mw.workspaces().count(), 2);
            assert_eq!(mw.workspace(), &workspace_b);
            assert_eq!(
                mw.project_group_keys(),
                vec![
                    ProjectGroupKey::new(None, PathList::new(&[path!("/project_b")])),
                    ProjectGroupKey::new(None, PathList::new(&[path!("/project_a")]))
                ]
            );
        })
        .unwrap();
    assert!(workspace_a.read_with(cx, |workspace, _cx| workspace.session_id().is_some()),);
}

#[gpui::test]
async fn test_close_workspace_with_remote_neighbor_does_not_create_local_workspace(
    cx: &mut TestAppContext,
) {
    // Regression test: closing a workspace whose neighboring group is
    // remote with no existing workspace should not create a local
    // workspace with the remote paths.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    let project_a = Project::test(fs, ["/root_a".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));

    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
    });
    cx.run_until_parked();

    // Add a mock-remote group with no workspace as the second group.
    let remote_key = ProjectGroupKey::new(
        Some(RemoteConnectionOptions::Mock(
            remote::MockConnectionOptions { id: 1 },
        )),
        PathList::new(&[PathBuf::from("/remote/project")]),
    );
    multi_workspace.update(cx, |mw, _cx| {
        mw.test_add_project_group(ProjectGroup {
            key: remote_key.clone(),
            workspaces: Vec::new(),
            expanded: true,
        });
    });

    let workspace_a = multi_workspace.read_with(cx, |mw, _cx| mw.workspace().clone());

    // Close workspace A. The neighbor is the remote group with no workspace.
    // The fix should skip find_or_create_local_workspace and fall through
    // to creating an empty workspace instead.
    multi_workspace
        .update_in(cx, |mw, window, cx| {
            mw.close_workspace(&workspace_a, window, cx)
        })
        .await
        .expect("close_workspace should succeed");

    cx.run_until_parked();

    multi_workspace.update(cx, |mw, cx| {
        // The active workspace should NOT be a local workspace with the
        // remote paths. It should be an empty workspace (no worktrees).
        let workspaces: Vec<_> = mw.workspaces().cloned().collect();
        for ws in &workspaces {
            let key = ws.read(cx).project_group_key(cx);
            assert!(
                key.host().is_some()
                    || key.path_list().paths() != [PathBuf::from("/remote/project")],
                "remote neighbor should not have created a local workspace"
            );
        }
    });
}

#[gpui::test]
async fn test_remove_project_group_with_remote_neighbor_does_not_create_local_workspace(
    cx: &mut TestAppContext,
) {
    // Regression test: removing a project group whose neighboring group is
    // remote with no workspace should not create a local workspace with
    // the remote paths.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/root_a", json!({ "file.txt": "" })).await;
    let project_a = Project::test(fs, ["/root_a".as_ref()], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));

    multi_workspace.update(cx, |mw, cx| {
        mw.open_sidebar(cx);
    });
    cx.run_until_parked();

    let key_a = project_a.read_with(cx, |p, cx| p.project_group_key(cx));

    // Add a mock-remote group with no workspace.
    let remote_key = ProjectGroupKey::new(
        Some(RemoteConnectionOptions::Mock(
            remote::MockConnectionOptions { id: 1 },
        )),
        PathList::new(&[PathBuf::from("/remote/project")]),
    );
    multi_workspace.update(cx, |mw, _cx| {
        mw.test_add_project_group(ProjectGroup {
            key: remote_key.clone(),
            workspaces: Vec::new(),
            expanded: true,
        });
    });

    // Remove the local group A. The neighbor is the remote group with no
    // workspace. The fix should skip find_or_create_local_workspace and
    // fall through to creating an empty workspace.
    multi_workspace
        .update_in(cx, |mw, window, cx| {
            mw.remove_project_group(&key_a, window, cx)
        })
        .await
        .expect("remove_project_group should succeed");

    cx.run_until_parked();

    multi_workspace.update(cx, |mw, cx| {
        let workspaces: Vec<_> = mw.workspaces().cloned().collect();
        for ws in &workspaces {
            let key = ws.read(cx).project_group_key(cx);
            assert!(
                key.host().is_some() || key.path_list().paths() != [PathBuf::from("/remote/project")],
                "remote neighbor should not have created a local workspace after remove_project_group"
            );
        }
    });
}
