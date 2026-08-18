use super::*;
use agent_workspaces::terminal_thread_metadata_store::{
    TerminalThreadMetadata, TerminalThreadMetadataStore,
};
use chrono::DateTime;
use fs::{FakeFs, Fs};
use gpui::TestAppContext;
use pretty_assertions::assert_eq;
use project::WorktreePaths;
use settings::SettingsStore;
use std::rc::Rc;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
    },
};
use util::path_list::PathList;

#[test]
fn test_renamed_worktree_path() {
    let old_path = Path::new("/worktrees/project/old");
    assert_eq!(
        renamed_worktree_path(old_path, " new "),
        Some(PathBuf::from("/worktrees/project/new"))
    );
    for invalid_name in ["", ".", "..", "nested/name", "nested\\name"] {
        assert_eq!(renamed_worktree_path(old_path, invalid_name), None);
    }
}

#[test]
fn test_cached_worktree_path_supports_consecutive_renames() {
    let repository_path = PathBuf::from("/project/.git");
    let mut available_worktrees = HashMap::from([(
        (repository_path.clone(), None),
        vec![git::repository::Worktree {
            path: PathBuf::from("/worktrees/feature"),
            ref_name: Some("refs/heads/feature".into()),
            sha: "abc".into(),
            is_main: false,
            is_bare: false,
        }],
    )]);

    update_cached_worktree_path(
        &mut available_worktrees,
        &repository_path,
        None,
        Path::new("/worktrees/feature"),
        Path::new("/worktrees/feature2"),
    );
    update_cached_worktree_path(
        &mut available_worktrees,
        &repository_path,
        None,
        Path::new("/worktrees/feature2"),
        Path::new("/worktrees/feature3"),
    );

    assert_eq!(
        available_worktrees[&(repository_path, None)][0].path,
        PathBuf::from("/worktrees/feature3")
    );
}

#[test]
fn test_stale_daemon_host_for_row() {
    use workspace_manager::{GroupId, ProjectId, RowKind, WorktreeId};

    let stale = |_: &str| true;
    let fresh = |_: &str| false;

    assert_eq!(
        stale_daemon_host_for_row(RowKind::Project(ProjectId(0)), Some("host"), stale),
        Some("host".to_owned())
    );
    assert_eq!(
        stale_daemon_host_for_row(RowKind::Project(ProjectId(0)), Some("host"), fresh),
        None
    );
    assert_eq!(
        stale_daemon_host_for_row(RowKind::Project(ProjectId(0)), None, stale),
        None
    );
    for kind in [
        RowKind::Group(GroupId(0)),
        RowKind::Worktree(WorktreeId(0)),
        RowKind::PinnedSection,
    ] {
        assert_eq!(
            stale_daemon_host_for_row(kind, Some("host"), |_| {
                panic!("a non-project row must not query daemon freshness")
            }),
            None
        );
    }
}

#[test]
fn test_cached_worktree_rename_is_scoped_to_remote_host() {
    let repository_path = PathBuf::from("/project/.git");
    let old_path = PathBuf::from("/worktrees/feature");
    let new_path = PathBuf::from("/worktrees/renamed");
    let worktree = git::repository::Worktree {
        path: old_path.clone(),
        ref_name: Some("refs/heads/feature".into()),
        sha: "abc".into(),
        is_main: false,
        is_bare: false,
    };
    let mut available_worktrees = HashMap::from([
        (
            (repository_path.clone(), Some("mock:host-a".to_owned())),
            vec![worktree.clone()],
        ),
        (
            (repository_path.clone(), Some("mock:host-b".to_owned())),
            vec![worktree],
        ),
    ]);

    update_cached_worktree_path(
        &mut available_worktrees,
        &repository_path,
        Some("mock:host-a"),
        &old_path,
        &new_path,
    );

    assert_eq!(
        available_worktrees[&(repository_path.clone(), Some("mock:host-a".to_owned()))][0].path,
        new_path,
    );
    assert_eq!(
        available_worktrees[&(repository_path, Some("mock:host-b".to_owned()))][0].path,
        old_path,
        "renaming a worktree on one host must not rewrite another host's same-path cache entry",
    );
}

#[test]
fn test_cached_worktree_rename_is_scoped_to_repository() {
    let selected_repository = PathBuf::from("/project-a/.git");
    let other_repository = PathBuf::from("/project-b/.git");
    let old_path = PathBuf::from("/worktrees/feature");
    let new_path = PathBuf::from("/worktrees/renamed");
    let worktree = git::repository::Worktree {
        path: old_path.clone(),
        ref_name: Some("refs/heads/feature".into()),
        sha: "abc".into(),
        is_main: false,
        is_bare: false,
    };
    let mut available_worktrees = HashMap::from([
        ((selected_repository.clone(), None), vec![worktree.clone()]),
        ((other_repository.clone(), None), vec![worktree]),
    ]);

    update_cached_worktree_path(
        &mut available_worktrees,
        &selected_repository,
        None,
        &old_path,
        &new_path,
    );

    assert_eq!(
        available_worktrees[&(selected_repository, None)][0].path,
        new_path,
    );
    assert_eq!(
        available_worktrees[&(other_repository, None)][0].path,
        old_path,
        "renaming one repository must not rewrite another repository's same-path cache entry",
    );
}

#[test]
fn test_open_group_filter_ignores_runtime_ssh_fields() {
    let runtime_host = RemoteConnectionOptions::Ssh(remote::SshConnectionOptions {
        host: "example.com".into(),
        username: Some("user".to_owned()),
        port: Some(2222),
        password: Some("secret".to_owned()),
        nickname: Some("current nickname".to_owned()),
        args: Some(vec!["-v".to_owned()]),
        connection_timeout: Some(30),
        upload_binary_over_ssh: true,
        ..Default::default()
    });
    let persisted_host = RemoteConnectionOptions::Ssh(remote::SshConnectionOptions {
        host: "example.com".into(),
        username: Some("user".to_owned()),
        port: Some(2222),
        ..Default::default()
    });
    let path_list = PathList::new(&[PathBuf::from("/project")]);
    let runtime_key = ProjectGroupKey::new(Some(runtime_host), path_list.clone());
    let persisted_key = ProjectGroupKey::new(Some(persisted_host), path_list);
    let unrelated =
        ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/unrelated-project")]));

    assert_eq!(
        closed_project_groups(&[runtime_key], vec![persisted_key, unrelated.clone()]),
        vec![unrelated],
        "a restored SSH key must not become a duplicate closed project because runtime-only options changed"
    );
}

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        // Use an isolated DB so parallel tests can't see each other's
        // persisted records (e.g. created-worktree records).
        cx.set_global(db::AppDatabase::test_new());
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        agent_workspaces::WorktreeMetadataStore::init_global(cx);
        TerminalThreadMetadataStore::init_global(cx);
    });
}

#[track_caller]
fn assert_project_header_has_entries(
    sidebar: &Entity<Sidebar>,
    project_name: &str,
    expected_has_entries: bool,
    cx: &mut gpui::VisualTestContext,
) {
    sidebar.read_with(cx, |sidebar, _cx| {
        let has_entries = sidebar.contents.entries.iter().find_map(|entry| {
            if let ListEntry::ProjectHeader {
                label, has_entries, ..
            } = entry
                && label.as_ref() == project_name
            {
                Some(*has_entries)
            } else {
                None
            }
        });

        assert_eq!(
            has_entries,
            Some(expected_has_entries),
            "expected project header `{project_name}` to have has_entries={expected_has_entries}, got {has_entries:?}"
        );
    });
}

async fn init_test_project(
    worktree_path: &str,
    cx: &mut TestAppContext,
) -> Entity<project::Project> {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(worktree_path, serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    project::Project::test(fs, [worktree_path.as_ref()], cx).await
}

fn setup_sidebar(
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut gpui::VisualTestContext,
) -> Entity<Sidebar> {
    let sidebar = setup_sidebar_closed(multi_workspace, cx);
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.toggle_sidebar(window, cx);
    });
    cx.run_until_parked();
    sidebar
}

fn setup_sidebar_closed(
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut gpui::VisualTestContext,
) -> Entity<Sidebar> {
    let multi_workspace = multi_workspace.clone();
    let sidebar =
        cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    multi_workspace.update(cx, |mw, cx| {
        mw.register_sidebar(sidebar.clone(), cx);
    });
    cx.run_until_parked();
    sidebar
}

#[gpui::test]
async fn test_footer_command_palette_button_dispatches_toggle(cx: &mut TestAppContext) {
    let project = init_test_project("/project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let toggles = Arc::new(AtomicUsize::new(0));
    workspace.update(cx, |workspace, _| {
        let toggles = toggles.clone();
        workspace.register_action(move |_, _: &zed_actions::command_palette::Toggle, _, _| {
            toggles.fetch_add(1, AtomicOrdering::Relaxed);
        });
    });
    let _sidebar = setup_sidebar(&multi_workspace, cx);
    cx.update(|window, cx| {
        window.refresh();
        let _ = window.draw(cx);
    });

    let button_bounds = cx
        .debug_bounds("sidebar-command-palette")
        .expect("the command palette button should render in the sidebar footer");
    cx.simulate_click(button_bounds.center(), gpui::Modifiers::none());

    assert_eq!(toggles.load(AtomicOrdering::Relaxed), 1);
}

fn submit_worktree_deletion(
    sidebar: &Entity<Sidebar>,
    root: &str,
    cx: &mut gpui::VisualTestContext,
) {
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.delete_worktree(PathBuf::from(root), None, None, None, window, cx);
    });
    assert!(
        cx.has_pending_prompt(),
        "worktree deletion should ask for confirmation"
    );
    cx.simulate_prompt_answer("Delete");
    cx.run_until_parked();
}

/// Opens a display-only terminal in the centre pane and returns its id.
fn insert_center_terminal(
    workspace: &Entity<Workspace>,
    project: &Entity<project::Project>,
    title: &str,
    created_at: DateTime<Utc>,
    cx: &mut gpui::VisualTestContext,
) -> TerminalId {
    let terminal_id = TerminalId::new();
    cx.update(|_, cx| {
        let worktree_paths = project.read(cx).worktree_paths(cx);
        let remote_connection = project.read(cx).remote_connection_options(cx);
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(
                TerminalThreadMetadata {
                    terminal_id,
                    title: title.to_string().into(),
                    custom_title: None,
                    created_at,
                    worktree_paths,
                    remote_connection,
                    working_directory: None,
                },
                cx,
            );
        });
    });
    workspace.update_in(cx, |workspace, window, cx| {
        terminal_view::terminal_panel::TerminalPanel::insert_test_center_terminal(
            workspace,
            terminal_id,
            window,
            cx,
        );
    });
    terminal_id
}

/// Rings the bell on the centre-pane terminal carrying `terminal_id`.
fn ring_center_terminal_bell(
    workspace: &Entity<Workspace>,
    terminal_id: TerminalId,
    cx: &mut gpui::VisualTestContext,
) {
    let view = workspace.read_with(cx, |workspace, cx| {
        workspace.panes().iter().find_map(|pane| {
            pane.read(cx)
                .items_of_type::<terminal_view::TerminalView>()
                .find(|view| view.read(cx).terminal_id() == Some(terminal_id))
        })
    });
    let view = view.expect("terminal should be live in the centre pane");
    view.update(cx, |view, cx| view.set_has_bell_for_test(true, cx));
}

/// Whether any centre pane still holds a terminal opened for `terminal_id`.
fn center_has_terminal(
    workspace: &Entity<Workspace>,
    terminal_id: TerminalId,
    cx: &mut gpui::VisualTestContext,
) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.panes().iter().any(|pane| {
            pane.read(cx)
                .items_of_type::<terminal_view::TerminalView>()
                .any(|view| view.read(cx).terminal_id() == Some(terminal_id))
        })
    })
}

/// The id of the terminal showing in the centre pane, if the active item is one.
fn active_center_terminal_id(
    workspace: &Entity<Workspace>,
    cx: &mut gpui::VisualTestContext,
) -> Option<TerminalId> {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_pane()
            .read(cx)
            .active_item()
            .and_then(|item| item.downcast::<terminal_view::TerminalView>())
            .and_then(|view| view.read(cx).terminal_id())
    })
}

/// Seeds a single named terminal row on `project`.
fn save_test_terminal(
    title: &str,
    created_at: DateTime<Utc>,
    project: &Entity<project::Project>,
    cx: &mut gpui::VisualTestContext,
) {
    cx.update(|_, cx| {
        let worktree_paths = project.read(cx).worktree_paths(cx);
        let remote_connection = project.read(cx).remote_connection_options(cx);
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(
                TerminalThreadMetadata {
                    terminal_id: TerminalId::new(),
                    title: title.to_string().into(),
                    custom_title: None,
                    created_at,
                    worktree_paths,
                    remote_connection,
                    working_directory: None,
                },
                cx,
            );
        });
    });
    cx.run_until_parked();
}

/// Seeds `count` terminal rows on `project`, titled "Terminal 1".."Terminal N"
/// with ascending creation times so their sidebar order is deterministic.
async fn save_n_test_terminals(
    count: u32,
    project: &Entity<project::Project>,
    cx: &mut gpui::VisualTestContext,
) {
    for i in 0..count {
        cx.update(|_, cx| {
            let worktree_paths = project.read(cx).worktree_paths(cx);
            let remote_connection = project.read(cx).remote_connection_options(cx);
            let metadata = TerminalThreadMetadata {
                terminal_id: TerminalId::new(),
                title: format!("Terminal {}", i + 1).into(),
                custom_title: None,
                created_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, i).unwrap(),
                worktree_paths,
                remote_connection,
                working_directory: None,
            };
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(metadata, cx);
            });
        });
    }
    cx.run_until_parked();
}

/// Spins up a fresh remote project backed by a headless server sharing
/// `server_fs`, opens the given worktree path on it, and returns the
/// project together with the headless entity (which the caller must keep
/// alive for the duration of the test) and the `RemoteConnectionOptions`
/// used for the fake server. Passing those options back into
/// `reuse_opts` on a subsequent call makes the new project share the
/// same `RemoteConnectionIdentity`, matching how Zed treats multiple
/// projects on the same SSH host.
async fn start_remote_project(
    server_fs: &Arc<FakeFs>,
    worktree_path: &Path,
    app_state: &Arc<workspace::AppState>,
    reuse_opts: Option<&remote::RemoteConnectionOptions>,
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (
    Entity<project::Project>,
    Entity<remote_server::HeadlessProject>,
    remote::RemoteConnectionOptions,
) {
    // Bare `_` on the guard so it's dropped immediately; holding onto it
    // would deadlock `connect_mock` below since the client waits on the
    // guard before completing the mock handshake.
    let (opts, server_session) = match reuse_opts {
        Some(existing) => {
            let (session, _) = remote::RemoteClient::fake_server_with_opts(existing, cx, server_cx);
            (existing.clone(), session)
        }
        None => {
            let (opts, session, _) = remote::RemoteClient::fake_server(cx, server_cx);
            (opts, session)
        }
    };

    server_cx.update(remote_server::HeadlessProject::init);
    let server_executor = server_cx.executor();
    let fs = server_fs.clone();
    let headless = server_cx.new(|cx| {
        remote_server::HeadlessProject::new(
            remote_server::HeadlessAppState {
                session: server_session,
                fs,
                http_client: Arc::new(http_client::BlockedHttpClient),
                node_runtime: node_runtime::NodeRuntime::unavailable(),
                languages: Arc::new(language::LanguageRegistry::new(server_executor.clone())),
                extension_host_proxy: Arc::new(extension::ExtensionHostProxy::new()),
                startup_time: std::time::Instant::now(),
            },
            false,
            cx,
        )
    });

    let remote_client = remote::RemoteClient::connect_mock(opts.clone(), cx).await;
    let project = cx.update(|cx| {
        let project_client = client::Client::new(
            Arc::new(clock::FakeSystemClock::new()),
            http_client::FakeHttpClient::with_404_response(),
            cx,
        );
        let user_store = cx.new(|cx| client::UserStore::new(project_client.clone(), cx));
        project::Project::remote(
            remote_client,
            project_client,
            node_runtime::NodeRuntime::unavailable(),
            user_store,
            app_state.languages.clone(),
            app_state.fs.clone(),
            false,
            cx,
        )
    });

    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(worktree_path, true, cx)
        })
        .await
        .expect("should open remote worktree");
    cx.run_until_parked();

    (project, headless, opts)
}

/// Seeds a terminal row whose folder paths differ from its main worktree
/// paths, i.e. a terminal living on a linked git worktree.
fn save_terminal_metadata_with_main_paths(
    title: &str,
    folder_paths: PathList,
    main_worktree_paths: PathList,
    created_at: DateTime<Utc>,
    cx: &mut TestAppContext,
) {
    let metadata = TerminalThreadMetadata {
        terminal_id: TerminalId::new(),
        title: title.to_string().into(),
        custom_title: None,
        created_at,
        worktree_paths: WorktreePaths::from_path_lists(main_worktree_paths, folder_paths).unwrap(),
        remote_connection: None,
        working_directory: None,
    };
    cx.update(|cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();
}

fn focus_sidebar(sidebar: &Entity<Sidebar>, cx: &mut gpui::VisualTestContext) {
    sidebar.update_in(cx, |_, window, cx| {
        cx.focus_self(window);
    });
    cx.run_until_parked();
}

fn format_linked_worktree_chips(worktrees: &[ThreadItemWorktreeInfo]) -> String {
    let mut seen = Vec::new();
    let mut chips = Vec::new();
    for wt in worktrees {
        if wt.kind == ui::WorktreeKind::Main {
            continue;
        }
        let Some(name) = wt.worktree_name.as_ref() else {
            continue;
        };
        if !seen.contains(name) {
            seen.push(name.clone());
            chips.push(format!("{{{}}}", name));
        }
    }
    if chips.is_empty() {
        String::new()
    } else {
        format!(" {}", chips.join(", "))
    }
}

fn visible_entries_as_strings(
    sidebar: &Entity<Sidebar>,
    cx: &mut gpui::VisualTestContext,
) -> Vec<String> {
    sidebar.read_with(cx, |sidebar, cx| {
        sidebar
            .contents
            .entries
            .iter()
            .enumerate()
            .map(|(ix, entry)| {
                let selected = if sidebar.selection == Some(ix) {
                    "  <== selected"
                } else {
                    ""
                };
                match entry {
                    ListEntry::ProjectHeader { label, key, .. } => {
                        let icon = if sidebar.is_group_collapsed(key, cx) {
                            ">"
                        } else {
                            "v"
                        };
                        format!("{} [{}]{}", icon, label, selected)
                    }
                    ListEntry::Terminal(terminal) => {
                        let title = terminal.metadata.display_title();
                        let worktree = format_linked_worktree_chips(&terminal.worktrees);
                        format!("  {title}{worktree}{selected}")
                    }
                }
            })
            .collect()
    })
}

#[gpui::test]
async fn test_thread_status_update_does_not_reset_list_measurements(cx: &mut TestAppContext) {
    // When a thread's status changes (e.g. Running -> Completed after sending a message), the
    // shape sequence is unchanged, so `update_entries` should not reset the underlying
    // `ListState`. Resetting throws away measured item bounds for one frame, which makes the
    // sticky project header flicker between its pushed-off and fully-on-screen positions.
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_terminals(2, &project, cx).await;
    cx.run_until_parked();

    let before = sidebar.read_with(cx, |sidebar, app| {
        sidebar
            .entry_shapes(multi_workspace.read(app))
            .collect::<Vec<_>>()
    });
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();
    let after = sidebar.read_with(cx, |sidebar, app| {
        sidebar
            .entry_shapes(multi_workspace.read(app))
            .collect::<Vec<_>>()
    });

    assert_eq!(
        before, after,
        "a no-op rebuild should produce an identical shape sequence"
    );
}

#[gpui::test]
async fn test_collapse_changes_entry_shape(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_terminals(2, &project, cx).await;
    cx.run_until_parked();

    let project_group_key = project.read_with(cx, |project, cx| project.project_group_key(cx));

    let before = sidebar.read_with(cx, |sidebar, app| {
        sidebar
            .entry_shapes(multi_workspace.read(app))
            .collect::<Vec<_>>()
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.toggle_collapse(&project_group_key, window, cx);
    });
    cx.run_until_parked();
    let after = sidebar.read_with(cx, |sidebar, app| {
        sidebar
            .entry_shapes(multi_workspace.read(app))
            .collect::<Vec<_>>()
    });

    assert_ne!(
        before, after,
        "collapsing the project group should change the shape sequence so the list resets"
    );
}

#[gpui::test]
async fn test_serialization_round_trip(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_terminals(3, &project, cx).await;

    let project_group_key = project.read_with(cx, |project, cx| project.project_group_key(cx));

    // Set a custom width and collapse the group.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.set_width(Some(px(420.0)), cx);
        sidebar.toggle_collapse(&project_group_key, window, cx);
    });
    cx.run_until_parked();

    // Capture the serialized state from the first sidebar.
    let serialized = sidebar.read_with(cx, |sidebar, cx| sidebar.serialized_state(cx));
    let serialized = serialized.expect("serialized_state should return Some");

    // Create a fresh sidebar and restore into it.
    let sidebar2 =
        cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    cx.run_until_parked();

    sidebar2.update_in(cx, |sidebar, window, cx| {
        sidebar.restore_serialized_state(&serialized, window, cx);
    });
    cx.run_until_parked();

    // Assert all serialized fields match.
    let width1 = sidebar.read_with(cx, |s, _| s.width);
    let width2 = sidebar2.read_with(cx, |s, _| s.width);

    assert_eq!(width1, width2);
    assert_eq!(width1, px(420.0));
}

#[gpui::test]
async fn test_entities_released_on_window_close(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let weak_workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().downgrade());
    let weak_sidebar = sidebar.downgrade();
    let weak_multi_workspace = multi_workspace.downgrade();

    drop(sidebar);
    drop(multi_workspace);
    cx.update(|window, _cx| window.remove_window());
    cx.run_until_parked();

    weak_multi_workspace.assert_released();
    weak_sidebar.assert_released();
    weak_workspace.assert_released();
}

#[gpui::test]
async fn test_single_workspace_no_threads(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);

    assert_eq!(
        visible_entries_as_strings(&_sidebar, cx),
        vec!["v [my-project]"]
    );
}

#[gpui::test]
async fn test_workspace_lifecycle(cx: &mut TestAppContext) {
    let project = init_test_project("/project-a", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Single workspace with a thread
    save_test_terminal(
        "Terminal A1",
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        &project,
        cx,
    );
    cx.run_until_parked();

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project-a]",
            "  Terminal A1",
        ]
    );

    // Add a second workspace
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.create_test_workspace(window, cx).detach();
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project-a]",
            "  Terminal A1",
        ]
    );
}

#[gpui::test]
async fn test_collapse_and_expand_group(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_terminals(1, &project, cx).await;

    let project_group_key = project.read_with(cx, |project, cx| project.project_group_key(cx));

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Terminal 1",
        ]
    );

    // Collapse
    sidebar.update_in(cx, |s, window, cx| {
        s.toggle_collapse(&project_group_key, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "> [my-project]",
        ]
    );

    // Expand
    sidebar.update_in(cx, |s, window, cx| {
        s.toggle_collapse(&project_group_key, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Terminal 1",
        ]
    );
}

#[gpui::test]
async fn test_collapse_state_survives_worktree_key_change(cx: &mut TestAppContext) {
    // When a worktree is added to a project, the project group key changes.
    // The sidebar's collapsed/expanded state is keyed by ProjectGroupKey, so
    // UI state must survive the key change.
    let (_fs, project) = init_multi_project_test(&["/project-a", "/project-b"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_terminals(2, &project, cx).await;
    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [project-a]", "  Terminal 2", "  Terminal 1",]
    );

    // Collapse the group.
    let old_key = project.read_with(cx, |project, cx| project.project_group_key(cx));
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.toggle_collapse(&old_key, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["> [project-a]"]
    );

    // Add a second worktree — the key changes from [/project-a] to
    // [/project-a, /project-b].
    project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/project-b", true, cx)
        })
        .await
        .expect("should add worktree");
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, _window, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    // The group should still be collapsed under the new key.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["> [project-a, project-b]"]
    );
}

#[gpui::test]
async fn test_keyboard_select_next_and_previous(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_terminals(3, &project, cx).await;

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // The cursor moves over the rows the workspace manager draws — a project
    // and its worktree — not over terminals, which are not rows of the tree.
    focus_sidebar(&sidebar, cx);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);

    // First SelectNext from None starts at the first row.
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));

    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));

    // At the end, wraps back to the first row.
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));

    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));

    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));

    // At the top, selection clears (focus returns to the editor).
    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);
}

#[gpui::test]
async fn test_keyboard_select_first_and_last(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_terminals(3, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);

    // SelectLast jumps to the last row the tree draws.
    cx.dispatch_action(SelectLast);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));

    // SelectFirst jumps to the beginning
    cx.dispatch_action(SelectFirst);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));
}

#[gpui::test]
async fn test_keyboard_focus_in_does_not_set_selection(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Initially no selection
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);

    // Open the sidebar so it's rendered, then focus it to trigger focus_in.
    // focus_in no longer sets a default selection.
    focus_sidebar(&sidebar, cx);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);

    // Manually set a selection, blur, then refocus — selection should be preserved
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(0);
    });

    cx.update(|window, _cx| {
        window.blur();
    });
    cx.run_until_parked();

    sidebar.update_in(cx, |_, window, cx| {
        cx.focus_self(window);
    });
    cx.run_until_parked();
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));
}

#[gpui::test]
async fn test_keyboard_confirm_on_project_header_toggles_collapse(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_terminals(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Terminal 1",
        ]
    );

    // Focus the sidebar and select the header
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(0);
    });

    // Confirm on a project row collapses it. Asserted on the group's own
    // state rather than the rendered strings: the cursor indexes tree rows,
    // which are not the same list `visible_entries_as_strings` reports.
    let rows_before = sidebar.update(cx, |sidebar, cx| sidebar.workspace_tree(cx).rows().len());
    cx.dispatch_action(Confirm);
    cx.run_until_parked();

    let rows_after = sidebar.update(cx, |sidebar, cx| sidebar.workspace_tree(cx).rows().len());
    assert!(
        rows_after < rows_before,
        "confirming a project row should collapse it, hiding its children \
         (rows went {rows_before} -> {rows_after})"
    );

    // Confirm again expands the group
    cx.dispatch_action(Confirm);
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]  <== selected",
            "  Terminal 1",
        ]
    );
}

#[gpui::test]
async fn test_keyboard_expand_and_collapse_selected_entry(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_terminals(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Terminal 1",
        ]
    );

    // Focus sidebar and manually select the header (index 0). Press left to collapse.
    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(0);
    });

    cx.dispatch_action(SelectParent);
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "> [my-project]  <== selected",
        ]
    );

    // Press right to expand
    cx.dispatch_action(SelectChild);
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]  <== selected",
            "  Terminal 1",
        ]
    );

    // Press right again on already-expanded header moves selection down
    cx.dispatch_action(SelectChild);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));
}

#[gpui::test]
async fn test_keyboard_collapse_from_child_selects_parent(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_terminals(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Focus sidebar (selection starts at None), then navigate down to the thread (child)
    focus_sidebar(&sidebar, cx);
    cx.dispatch_action(SelectNext);
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Terminal 1  <== selected",
        ]
    );

    // Pressing left on a child collapses the parent group and selects it
    cx.dispatch_action(SelectParent);
    cx.run_until_parked();

    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "> [my-project]  <== selected",
        ]
    );
}

#[gpui::test]
async fn test_keyboard_navigation_on_empty_list(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/empty-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // An empty project has only the header (no auto-created draft).
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [empty-project]"]
    );

    // Focus sidebar — focus_in does not set a selection
    focus_sidebar(&sidebar, cx);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);

    // First SelectNext from None starts at index 0 (header)
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));

    // A project with no terminals still draws its worktree row.
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));

    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(0));

    // SelectPrevious from first entry clears selection (returns to editor)
    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), None);

    // SelectPrevious from None selects the last row. A project with no
    // terminals still draws a project row and its worktree.
    cx.dispatch_action(SelectPrevious);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));
}

#[gpui::test]
async fn test_new_entry_noops_without_open_project(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
    let project = project::Project::test(fs, [], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_new_entry(&workspace, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        Vec::<String>::new()
    );
}

#[gpui::test]
async fn test_selection_clamps_after_entry_removal(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_n_test_terminals(1, &project, cx).await;
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Focus sidebar (selection starts at None), navigate down to the thread (index 1)
    focus_sidebar(&sidebar, cx);
    cx.dispatch_action(SelectNext);
    cx.dispatch_action(SelectNext);
    assert_eq!(sidebar.read_with(cx, |s, _| s.selection), Some(1));

    // Collapse the group, which removes the thread from the list
    cx.dispatch_action(SelectParent);
    cx.run_until_parked();

    // Selection should be clamped to the last valid index (0 = header)
    let selection = sidebar.read_with(cx, |s, _| s.selection);
    let entry_count = sidebar.read_with(cx, |s, _| s.contents.entries.len());
    assert!(
        selection.unwrap_or(0) < entry_count,
        "selection {} should be within bounds (entries: {})",
        selection.unwrap_or(0),
        entry_count,
    );
}

async fn init_test_project_with_agent_panel(
    worktree_path: &str,
    cx: &mut TestAppContext,
) -> Entity<project::Project> {
    agent_workspaces::test_support::init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(worktree_path, serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    project::Project::test(fs, [worktree_path.as_ref()], cx).await
}

#[gpui::test]
async fn test_agent_panel_terminals_appear_in_sidebar_and_search(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    let terminal_id = insert_center_terminal(
        &workspace,
        &project,
        "Dev Server",
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  Dev Server"]
    );
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Terminal { terminal_id: active_terminal_id, .. }) if *active_terminal_id == terminal_id),
            "expected active terminal entry, got {:?}",
            sidebar.active_entry,
        );
        assert!(
            sidebar.contents.entries.iter().any(|entry| {
                matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id && terminal.metadata.display_title().as_ref() == "Dev Server")
            }),
            "expected the inserted terminal to appear in sidebar contents",
        );
    });
    sidebar.read_with(cx, |_sidebar, cx| {
        let store = TerminalThreadMetadataStore::global(cx).read(cx);
        let metadata = store
            .entry(terminal_id)
            .expect("terminal metadata should be persisted");
        assert_eq!(metadata.title.as_ref(), "Dev Server");
        assert_eq!(metadata.custom_title, None);
        assert_eq!(metadata.display_title().as_ref(), "Dev Server");
        assert!(
            metadata
                .folder_paths()
                .paths()
                .iter()
                .any(|path| path.as_path() == Path::new("/my-project"))
        );
    });

    type_in_search(&sidebar, "server", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]", "  Dev Server  <== selected"]
    );

    type_in_search(&sidebar, "missing", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        Vec::<String>::new()
    );
}

#[gpui::test]
async fn test_closing_last_agent_panel_terminal_restores_empty_header(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    assert_project_header_has_entries(&sidebar, "my-project", false, cx);

    let terminal_id = insert_center_terminal(
        &workspace,
        &project,
        "Dev Server",
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    cx.run_until_parked();

    assert_project_header_has_entries(&sidebar, "my-project", true, cx);

    let (terminal_metadata, terminal_workspace) = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .find_map(|entry| match entry {
                ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id => {
                    Some((terminal.metadata.clone(), terminal.workspace.clone()))
                }
                _ => None,
            })
            .expect("terminal should be visible in sidebar")
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.close_terminal(&terminal_metadata, &terminal_workspace, window, cx);
    });
    cx.run_until_parked();

    assert!(
        !center_has_terminal(&workspace, terminal_id, cx),
        "closing from the sidebar should remove the terminal from the centre pane"
    );
    // Closing the last terminal leaves the group header alone: there is no
    // other kind of entry left to fall back to.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [my-project]"]
    );

    let project_group_key = multi_workspace.read_with(cx, |multi_workspace, cx| {
        multi_workspace.workspace().read(cx).project_group_key(cx)
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.toggle_collapse(&project_group_key, window, cx);
    });
    cx.run_until_parked();

    // Collapsed: the header hides its children, and with the last terminal
    // closed there are no entries left to report.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["> [my-project]"]
    );
    assert_project_header_has_entries(&sidebar, "my-project", false, cx);
}

#[gpui::test]
async fn test_terminal_metadata_is_deduped_across_project_groups(cx: &mut TestAppContext) {
    agent_workspaces::test_support::init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project-a", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/project-b", serde_json::json!({ "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/project-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/project-b".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace_a = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(project_b, window, cx);
    });
    let terminal_id = TerminalId::new();
    workspace_a.update_in(cx, |workspace, window, cx| {
        terminal_view::terminal_panel::TerminalPanel::insert_test_center_terminal(
            workspace,
            terminal_id,
            window,
            cx,
        );
    });
    cx.run_until_parked();

    workspace_a.update_in(cx, |workspace, window, cx| {
        terminal_view::terminal_panel::TerminalPanel::close_center_terminal(
            workspace,
            terminal_id,
            window,
            cx,
        );
    });
    let now = Utc::now();
    let metadata = TerminalThreadMetadata {
        terminal_id,
        title: "Dev Server".into(),
        custom_title: None,
        created_at: now,
        worktree_paths: WorktreePaths::from_path_lists(
            PathList::new(&[PathBuf::from("/project-a")]),
            PathList::new(&[PathBuf::from("/project-b")]),
        )
        .unwrap(),
        remote_connection: None,
        working_directory: None,
    };

    cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(metadata, cx);
        });
    });
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(
            sidebar
                .contents
                .entries
                .iter()
                .filter(|entry| {
                    matches!(
                        entry,
                        ListEntry::Terminal(terminal)
                            if terminal.metadata.terminal_id == terminal_id
                    )
                })
                .count(),
            1
        );
    });
}

#[gpui::test]
async fn test_agent_panel_terminal_shows_project_and_linked_worktree(cx: &mut TestAppContext) {
    agent_workspaces::test_support::init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;

    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let worktree_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(worktree_project.clone(), window, cx)
    });
    insert_center_terminal(
        &worktree_workspace,
        &worktree_project,
        "Dev Server",
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [project]", "  Dev Server {wt-feature-a}"]
    );

    type_in_search(&sidebar, "wt-feature-a", cx);
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [project]", "  Dev Server {wt-feature-a}  <== selected"]
    );
}

#[gpui::test]
async fn test_agent_panel_terminal_notifications_update_sidebar(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    let build_terminal_id = insert_center_terminal(
        &workspace,
        &project,
        "Build",
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    let _server_terminal_id = insert_center_terminal(
        &workspace,
        &project,
        "Server",
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 1).unwrap(),
        cx,
    );
    cx.run_until_parked();

    ring_center_terminal_bell(&workspace, build_terminal_id, cx);
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, cx| {
        assert!(sidebar.has_notifications(cx));
        assert!(sidebar.contents.notified_terminals.contains(&build_terminal_id));
        assert!(sidebar.contents.entries.iter().any(|entry| {
            matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == build_terminal_id && terminal.has_notification)
        }));
    });

    // Clearing is not asserted here: a centre-pane terminal drops its bell
    // when the view takes focus, and focus does not propagate in this
    // harness the way it does on screen.
}

#[gpui::test]
async fn test_thread_switcher_can_activate_agent_panel_terminal(cx: &mut TestAppContext) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    let build_terminal_id = insert_center_terminal(
        &workspace,
        &project,
        "Build",
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    let server_terminal_id = insert_center_terminal(
        &workspace,
        &project,
        "Server",
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 1).unwrap(),
        cx,
    );
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    let (entry_terminal_ids, selected_terminal_id) = sidebar.read_with(cx, |sidebar, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        let switcher = switcher.read(cx);
        let entry_terminal_ids = switcher
            .entries()
            .iter()
            .map(|entry| {
                entry
                    .terminal_id()
                    .expect("expected terminal switcher entry")
            })
            .collect::<Vec<_>>();
        let selected_terminal_id = switcher
            .selected_entry()
            .expect("switcher should have selected entry")
            .terminal_id()
            .expect("expected selected terminal switcher entry");
        (entry_terminal_ids, selected_terminal_id)
    });

    assert_eq!(entry_terminal_ids.len(), 2);
    assert!(entry_terminal_ids.contains(&build_terminal_id));
    assert!(entry_terminal_ids.contains(&server_terminal_id));

    sidebar.update_in(cx, |sidebar, window, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        let focus = switcher.focus_handle(cx);
        focus.dispatch_action(&menu::Confirm, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        active_center_terminal_id(&workspace, cx),
        Some(selected_terminal_id),
        "confirming in the switcher should bring that terminal to the centre pane"
    );
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            matches!(&sidebar.active_entry, Some(ActiveEntry::Terminal { terminal_id, .. }) if *terminal_id == selected_terminal_id),
            "expected selected terminal to become active, got {:?}",
            sidebar.active_entry,
        );
    });
}

#[gpui::test]
async fn test_thread_switcher_includes_terminal_metadata_for_open_project_group(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // A terminal that is no longer live: only its stored row remains.
    let terminal_id = TerminalId::new();
    save_test_terminal(
        "Newer Terminal",
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 3, 0, 0, 0).unwrap(),
        &project,
        cx,
    );
    save_test_terminal(
        "Older Terminal",
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        &project,
        cx,
    );

    let created_at = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap();
    let metadata = TerminalThreadMetadata {
        terminal_id,
        title: "Feature Terminal".into(),
        custom_title: None,
        created_at,
        worktree_paths: WorktreePaths::from_path_lists(
            PathList::new(&[PathBuf::from("/project")]),
            PathList::new(&[PathBuf::from("/project-feature")]),
        )
        .unwrap(),
        remote_connection: None,
        working_directory: None,
    };
    cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(metadata, cx);
        });
    });
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        assert!(
            switcher
                .read(cx)
                .entries()
                .iter()
                .any(|entry| entry.terminal_id() == Some(terminal_id)),
            "terminal metadata row should be included like a closed thread row"
        );
    });
}

#[gpui::test]
async fn test_thread_switcher_preserves_closed_terminal_linked_worktree_workspace(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/project/feature-a/project",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/project/feature-a/project"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    let terminal_id = TerminalId::new();
    workspace.update_in(cx, |workspace, window, cx| {
        terminal_view::terminal_panel::TerminalPanel::insert_test_center_terminal(
            workspace,
            terminal_id,
            window,
            cx,
        );
        terminal_view::terminal_panel::TerminalPanel::close_center_terminal(
            workspace,
            terminal_id,
            window,
            cx,
        );
    });
    let created_at = chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap();
    let worktree_folder_paths =
        PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]);
    let metadata = TerminalThreadMetadata {
        terminal_id,
        title: "Feature Terminal".into(),
        custom_title: None,
        created_at,
        worktree_paths: WorktreePaths::from_path_lists(
            PathList::new(&[PathBuf::from("/project")]),
            worktree_folder_paths.clone(),
        )
        .unwrap(),
        remote_connection: None,
        working_directory: None,
    };
    cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(metadata, cx);
        });
    });
    save_test_terminal(
        "Main Terminal",
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        &main_project,
        cx,
    );
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(&worktree_folder_paths, None, cx)
            })
            .is_none(),
        "linked worktree workspace should start closed"
    );

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.on_toggle_thread_switcher(&ToggleThreadSwitcher::default(), window, cx);
    });
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, cx| {
        let switcher = sidebar
            .thread_switcher
            .as_ref()
            .expect("switcher should be open");
        match switcher
            .read(cx)
            .selected_entry()
            .expect("switcher should select the terminal row by default")
        {
            ThreadSwitcherEntry::Terminal(entry) => {
                assert_eq!(entry.metadata.terminal_id, terminal_id);
                match &entry.workspace {
                    ThreadEntryWorkspace::Closed {
                        folder_paths,
                        project_group_key,
                    } => {
                        assert_eq!(folder_paths, &worktree_folder_paths);
                        assert_eq!(
                            project_group_key.path_list(),
                            &PathList::new(&[PathBuf::from("/project")])
                        );
                    }
                    ThreadEntryWorkspace::Open(_) => {
                        panic!("closed terminal row should retain its linked worktree target")
                    }
                }
            }
        }
    });
}

#[gpui::test]
async fn test_delete_linked_worktrees_does_not_create_phantom_project_groups(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                    "feature-b": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-b",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    for (name, branch) in [("feature-a", "feature-a"), ("feature-b", "feature-b")] {
        let root = format!("/worktrees/project/{name}/project");
        fs.insert_tree(
            &root,
            serde_json::json!({
                ".git": format!("gitdir: /project/.git/worktrees/{name}"),
                "src": {},
            }),
        )
        .await;
        fs.add_linked_worktree_for_repo(
            Path::new("/project/.git"),
            false,
            git::repository::Worktree {
                path: PathBuf::from(&root),
                ref_name: Some(format!("refs/heads/{branch}").into()),
                sha: format!("sha-{name}").into(),
                is_main: false,
                is_bare: false,
            },
        )
        .await;
    }
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let project_a = project::Project::test(
        fs.clone(),
        ["/worktrees/project/feature-a/project".as_ref()],
        cx,
    )
    .await;
    let project_b = project::Project::test(
        fs.clone(),
        ["/worktrees/project/feature-b/project".as_ref()],
        cx,
    )
    .await;
    for project in [&main_project, &project_a, &project_b] {
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
    }

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(project_a, window, cx);
        multi_workspace.test_add_workspace(project_b, window, cx);
    });
    cx.run_until_parked();

    let main_key = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/project")]));
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| {
            multi_workspace.project_group_keys()
        }),
        vec![main_key.clone()]
    );

    submit_worktree_deletion(&sidebar, "/worktrees/project/feature-a/project", cx);
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| {
            multi_workspace.project_group_keys()
        }),
        vec![main_key.clone()],
        "deleting the first worktree must not create a project named after it"
    );

    submit_worktree_deletion(&sidebar, "/worktrees/project/feature-b/project", cx);
    multi_workspace.read_with(cx, |multi_workspace, _| {
        assert_eq!(multi_workspace.project_group_keys(), vec![main_key]);
        assert_eq!(multi_workspace.workspaces().count(), 1);
    });
}

#[gpui::test]
async fn test_delete_linked_root_keeps_the_other_root_in_a_multi_root_workspace(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let linked_root = PathBuf::from("/worktrees/feature");
    fs.insert_tree("/project", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    fs.insert_tree(
        &linked_root,
        serde_json::json!({ ".git": "gitdir: /project/.git/worktrees/feature", "src": {} }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: linked_root.clone(),
            ref_name: Some("refs/heads/feature".into()),
            sha: "abc".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = project::Project::test(
        fs.clone(),
        [Path::new("/project"), linked_root.as_path()],
        cx,
    )
    .await;
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    submit_worktree_deletion(
        &sidebar,
        linked_root.to_str().expect("test path is valid UTF-8"),
        cx,
    );

    assert!(!fs.is_dir(&linked_root).await);
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        1
    );
    let roots = project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .collect::<Vec<_>>()
    });
    assert_eq!(roots, vec![PathBuf::from("/project")]);
}

#[gpui::test]
async fn test_close_selected_linked_worktree_closes_its_workspace_not_the_active_main_workspace(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let main_root = PathBuf::from("/project");
    let linked_root = PathBuf::from("/worktrees/feature");
    fs.insert_tree(&main_root, serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    fs.insert_tree(
        &linked_root,
        serde_json::json!({ ".git": "gitdir: /project/.git/worktrees/feature", "src": {} }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: linked_root.clone(),
            ref_name: Some("refs/heads/feature".into()),
            sha: "abc".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), [main_root.as_path()], cx).await;
    let linked_project = project::Project::test(fs, [linked_root.as_path()], cx).await;
    for project in [&main_project, &linked_project] {
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
    }
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project, window, cx));
    let main_workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(linked_project, window, cx);
        multi_workspace.activate(main_workspace.clone(), None, window, cx);
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let build_menu = sidebar.update(cx, |sidebar, cx| {
        let tree = sidebar.workspace_tree(cx);
        let worktree_id = tree
            .groups
            .iter()
            .flat_map(|group| &group.projects)
            .flat_map(|project| &project.worktrees)
            .find(|worktree| worktree.folder_root.as_deref() == Some(linked_root.as_path()))
            .expect("linked worktree row should exist")
            .id;
        let context =
            sidebar.workspace_row_context(&tree, workspace_manager::RowKind::Worktree(worktree_id));
        sidebar.workspace_manager_row_menu(context, cx)
    });
    let menu = cx.update(|window, cx| build_menu(window, cx));
    menu.update_in(cx, |menu, window, cx| {
        menu.select_last(window, cx);
        menu.select_previous(&SelectPrevious, window, cx);
        menu.confirm(&Confirm, window, cx);
    });
    cx.run_until_parked();

    multi_workspace.read_with(cx, |multi_workspace, _| {
        assert!(
            multi_workspace
                .workspaces()
                .any(|workspace| workspace == &main_workspace),
            "closing a linked row must not close whichever workspace happens to be active"
        );
        assert_eq!(multi_workspace.workspaces().count(), 1);
    });
    sidebar.update(cx, |sidebar, cx| {
        let tree = sidebar.workspace_tree(cx);
        let closed_row = tree
            .groups
            .iter()
            .flat_map(|group| &group.projects)
            .flat_map(|project| &project.worktrees)
            .find(|worktree| worktree.folder_root.as_deref() == Some(linked_root.as_path()))
            .expect("closing a workspace must not pretend the Git worktree was deleted");
        assert!(closed_row.workspace.is_none());
    });
}

#[gpui::test]
async fn test_persistent_workspace_reset_requires_confirmation_and_cancel_can_retry(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let root = PathBuf::from("/project");
    fs.insert_tree(&root, serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = project::Project::test(fs, [root.as_path()], cx).await;
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let open_reset_prompt = |cx: &mut gpui::VisualTestContext| {
        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.kill_and_recreate_workspace_sessions(
                workspace.downgrade(),
                "project".into(),
                PathBuf::from("/project"),
                None,
                window,
                cx,
            );
        });
    };

    open_reset_prompt(cx);
    assert!(
        cx.pending_prompt().is_some_and(|(title, detail)| {
            title.contains("Kill and recreate sessions")
                && detail.contains("Repository files and other worktrees are not changed")
        }),
        "the destructive action must explain its exact scope"
    );
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert!(!cx.has_pending_prompt());

    open_reset_prompt(cx);
    assert!(
        cx.has_pending_prompt(),
        "cancelling must leave the recovery action available"
    );
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
}

#[gpui::test]
async fn test_rename_linked_worktree_does_not_create_phantom_project_group(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let main_path = PathBuf::from("/project");
    let old_path = PathBuf::from("/worktrees/project/feature/project");
    let new_path = PathBuf::from("/worktrees/project/feature/renamed");
    let sibling_path = PathBuf::from("/worktrees/project/sibling/project");
    fs.insert_tree(
        &main_path,
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(&old_path, serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree(&sibling_path, serde_json::json!({ "src": {} }))
        .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: old_path.clone(),
            ref_name: Some("refs/heads/feature".into()),
            sha: "abc".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: sibling_path.clone(),
            ref_name: Some("refs/heads/sibling".into()),
            sha: "def".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), [main_path.as_path()], cx).await;
    let linked_project =
        project::Project::test(fs.clone(), [old_path.as_path(), sibling_path.as_path()], cx).await;
    let duplicate_linked_project =
        project::Project::test(fs.clone(), [old_path.as_path()], cx).await;
    for project in [&main_project, &linked_project, &duplicate_linked_project] {
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
    }

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(linked_project.clone(), window, cx);
        multi_workspace.test_add_workspace(duplicate_linked_project.clone(), window, cx);
    });
    sidebar.update(cx, |sidebar, _| {
        for paths in [
            &mut sidebar.pinned_worktrees,
            &mut sidebar.unread_worktrees,
            &mut sidebar.hidden_worktrees,
        ] {
            paths.extend([
                workspace_manager::ScopedPath::new(old_path.clone(), None),
                workspace_manager::ScopedPath::new(old_path.clone(), None),
            ]);
        }
    });
    cx.run_until_parked();

    let linked_worktree = linked_project.read_with(cx, |project, cx| {
        let worktree = project
            .visible_worktrees(cx)
            .next()
            .expect("linked worktree should be open");
        worktree
    });
    let linked_worktree_id = linked_worktree.read_with(cx, |worktree, _| worktree.id());
    linked_worktree.update(cx, |worktree, cx| {
        worktree
            .as_local_mut()
            .expect("linked worktree should be local")
            .set_defer_watch(true, cx);
    });
    linked_project.update(cx, |project, cx| {
        assert!(project.update_worktree_abs_path(linked_worktree_id, &new_path, cx));
    });
    let available_worktrees =
        sidebar.read_with(cx, |sidebar, _| sidebar.available_worktrees.clone());
    let open_workspaces = multi_workspace.read_with(cx, |multi_workspace, _| {
        multi_workspace.workspaces().cloned().collect::<Vec<_>>()
    });
    let tree = cx.update(|_, cx| {
        workspace_manager::build_tree(&open_workspaces, &available_worktrees, &[], cx)
    });
    assert_eq!(
        tree.groups[0].projects.len(),
        1,
        "a moved worktree must stay under its root repository while its repository model catches up"
    );
    linked_project.update(cx, |project, cx| {
        assert!(project.update_worktree_abs_path(linked_worktree_id, &old_path, cx));
    });

    let renamed_terminal_id = TerminalId::new();
    let sibling_terminal_id = TerminalId::new();
    let other_host_terminal_id = TerminalId::new();
    let other_host = RemoteConnectionOptions::Ssh(remote::SshConnectionOptions {
        host: "other-host".into(),
        ..Default::default()
    });
    let renamed_worktree_paths =
        linked_project.read_with(cx, |project, cx| project.worktree_paths(cx));
    cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            for metadata in [
                TerminalThreadMetadata {
                    terminal_id: renamed_terminal_id,
                    title: "renamed".into(),
                    custom_title: None,
                    created_at: Utc::now(),
                    worktree_paths: renamed_worktree_paths.clone(),
                    remote_connection: None,
                    working_directory: Some(old_path.join("src/bin")),
                },
                TerminalThreadMetadata {
                    terminal_id: sibling_terminal_id,
                    title: "unchanged sibling".into(),
                    custom_title: None,
                    created_at: Utc::now(),
                    worktree_paths: renamed_worktree_paths.clone(),
                    remote_connection: None,
                    working_directory: Some(sibling_path.join("src")),
                },
                TerminalThreadMetadata {
                    terminal_id: other_host_terminal_id,
                    title: "other host".into(),
                    custom_title: None,
                    created_at: Utc::now(),
                    worktree_paths: renamed_worktree_paths.clone(),
                    remote_connection: Some(other_host.clone()),
                    working_directory: Some(old_path.join("remote-src")),
                },
            ] {
                store.save(metadata, cx);
            }
        });
    });

    // A provisional or restored group can still use the checkout path as its
    // identity. It stays hidden only while an open workspace covers that path.
    let stale_key = ProjectGroupKey::new(None, PathList::new(std::slice::from_ref(&old_path)));
    multi_workspace.update(cx, |multi_workspace, _| {
        multi_workspace.test_add_project_group(workspace::ProjectGroup {
            key: stale_key.clone(),
            workspaces: Vec::new(),
            expanded: true,
        });
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.start_renaming_worktree(
            old_path.clone(),
            None,
            Some(PathBuf::from("/project/.git")),
            "feature".into(),
            window,
            cx,
        );
        sidebar.worktree_rename_editor.update(cx, |editor, cx| {
            editor.set_text("renamed", window, cx);
        });
    });
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.confirm(&Confirm, window, cx);
    });
    cx.run_until_parked();

    assert!(!fs.is_dir(&old_path).await);
    assert!(fs.is_dir(&new_path).await);
    linked_project.read_with(cx, |project, cx| {
        let paths = project
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path())
            .collect::<Vec<_>>();
        assert!(paths.iter().any(|path| path.as_ref() == new_path));
        assert!(paths.iter().any(|path| path.as_ref() == sibling_path));
    });
    duplicate_linked_project.read_with(cx, |project, cx| {
        let worktree_path = project
            .visible_worktrees(cx)
            .next()
            .expect("linked worktree should remain open")
            .read(cx)
            .abs_path();
        assert_eq!(worktree_path.as_ref(), new_path);
    });
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| {
            multi_workspace.project_group_keys()
        }),
        vec![ProjectGroupKey::new(
            None,
            PathList::new(std::slice::from_ref(&main_path)),
        )]
    );
    sidebar.read_with(cx, |sidebar, _| {
        for paths in [
            &sidebar.pinned_worktrees,
            &sidebar.unread_worktrees,
            &sidebar.hidden_worktrees,
        ] {
            assert_eq!(paths.len(), 2);
            assert!(paths.iter().all(|path| path.matches(&new_path, None)));
        }
    });
    let serialized = sidebar
        .read_with(cx, |sidebar, cx| sidebar.serialized_state(cx))
        .expect("renamed scoped state should serialize");
    let restored_sidebar =
        cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    restored_sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.restore_serialized_state(&serialized, window, cx);
    });
    cx.run_until_parked();
    restored_sidebar.read_with(cx, |sidebar, _| {
        for paths in [
            &sidebar.pinned_worktrees,
            &sidebar.unread_worktrees,
            &sidebar.hidden_worktrees,
        ] {
            assert_eq!(paths.len(), 2);
            assert!(paths.iter().all(|path| path.matches(&new_path, None)));
            assert!(paths.iter().all(|path| !path.matches(&old_path, None)));
        }
    });

    cx.update(|_, cx| {
        let store = TerminalThreadMetadataStore::global(cx);
        let store = store.read(cx);
        let renamed = store
            .entry(renamed_terminal_id)
            .expect("renamed terminal metadata should remain cached");
        assert_eq!(renamed.working_directory, Some(new_path.join("src/bin")));
        assert_eq!(
            renamed.folder_paths(),
            &PathList::new(&[new_path.clone(), sibling_path.clone()])
        );
        assert_eq!(
            store
                .entry(sibling_terminal_id)
                .expect("sibling terminal should remain cached")
                .working_directory,
            Some(sibling_path.join("src"))
        );
        assert_eq!(
            store
                .entry(other_host_terminal_id)
                .expect("other-host terminal should remain cached")
                .working_directory,
            Some(old_path.join("remote-src"))
        );
    });

    cx.run_until_parked();
    cx.update(|_, cx| TerminalThreadMetadataStore::init_global(cx));
    let reload = cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx)
            .read(cx)
            .reload_task()
    });
    reload.await;
    cx.update(|_, cx| {
        let store = TerminalThreadMetadataStore::global(cx);
        let store = store.read(cx);
        let renamed = store
            .entry(renamed_terminal_id)
            .expect("renamed terminal metadata should survive a DB reload");
        assert_eq!(renamed.working_directory, Some(new_path.join("src/bin")));
        assert_eq!(
            renamed.folder_paths(),
            &PathList::new(&[new_path, sibling_path.clone()])
        );
        assert_eq!(
            store
                .entry(sibling_terminal_id)
                .expect("sibling terminal should survive a DB reload")
                .working_directory,
            Some(sibling_path.join("src"))
        );
        assert_eq!(
            store
                .entry(other_host_terminal_id)
                .expect("other-host terminal should survive a DB reload")
                .working_directory,
            Some(old_path.join("remote-src"))
        );
    });
}

#[gpui::test]
async fn test_failed_worktree_renames_preserve_cache_group_and_scoped_state(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let main_root = PathBuf::from("/project");
    let old_path = PathBuf::from("/worktrees/feature");
    let occupied_path = PathBuf::from("/worktrees/occupied");
    let empty_path = PathBuf::from("/worktrees/empty");
    fs.insert_tree(&main_root, serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    fs.insert_tree(&occupied_path, serde_json::json!({ "keep": "me" }))
        .await;
    fs.create_dir(&empty_path)
        .await
        .expect("empty destination should exist");
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: old_path.clone(),
            ref_name: Some("refs/heads/feature".into()),
            sha: "abc".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = project::Project::test(fs.clone(), [main_root.as_path()], cx).await;
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let stale_key = ProjectGroupKey::new(None, PathList::new(std::slice::from_ref(&old_path)));
    multi_workspace.update(cx, |multi_workspace, _| {
        multi_workspace.test_add_project_group(workspace::ProjectGroup {
            key: stale_key.clone(),
            workspaces: Vec::new(),
            expanded: true,
        });
    });
    sidebar.update(cx, |sidebar, _| {
        for paths in [
            &mut sidebar.pinned_worktrees,
            &mut sidebar.unread_worktrees,
            &mut sidebar.hidden_worktrees,
        ] {
            paths.push(workspace_manager::ScopedPath::new(old_path.clone(), None));
        }
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.start_renaming_worktree(
            old_path.clone(),
            None,
            Some(PathBuf::from("/project/.git")),
            "feature".into(),
            window,
            cx,
        );
        sidebar.worktree_rename_editor.update(cx, |editor, cx| {
            editor.set_text("cancelled", window, cx);
        });
    });
    cx.run_until_parked();
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.cancel(&Cancel, window, cx);
    });
    cx.run_until_parked();
    assert!(fs.is_dir(&old_path).await);
    assert!(!fs.is_dir(Path::new("/worktrees/cancelled")).await);
    sidebar.read_with(cx, |sidebar, _| {
        assert!(sidebar.renaming_worktree.is_none());
        assert!(sidebar.renaming_worktree_name.is_none());
    });

    let submit_rename = |name: &str, cx: &mut gpui::VisualTestContext| {
        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.start_renaming_worktree(
                old_path.clone(),
                None,
                Some(PathBuf::from("/project/.git")),
                "feature".into(),
                window,
                cx,
            );
            sidebar.worktree_rename_editor.update(cx, |editor, cx| {
                editor.set_text(name, window, cx);
            });
            sidebar.commit_worktree_rename(window, cx);
        });
        cx.run_until_parked();
    };
    submit_rename(" nested/name ", cx);
    assert!(fs.is_dir(&old_path).await);
    assert!(!fs.is_dir(Path::new("/worktrees/nested")).await);

    submit_rename("empty", cx);
    assert!(fs.is_dir(&old_path).await);
    assert!(fs.is_dir(&empty_path).await);

    submit_rename("occupied", cx);

    assert!(fs.is_dir(&old_path).await);
    assert!(fs.is_file(&occupied_path.join("keep")).await);
    fs.remove_file(
        &old_path.join(".git"),
        fs::RemoveOptions {
            recursive: false,
            ignore_if_not_exists: false,
        },
    )
    .await
    .expect("second rename should exercise a missing-.git source");
    submit_rename("missing-source", cx);

    assert!(fs.is_dir(&old_path).await);
    assert!(!fs.is_dir(Path::new("/worktrees/missing-source")).await);
    multi_workspace.read_with(cx, |multi_workspace, _| {
        assert!(multi_workspace.project_group_keys().contains(&stale_key));
    });
    sidebar.read_with(cx, |sidebar, _| {
        assert!(
            sidebar
                .available_worktrees
                .values()
                .any(|worktrees| { worktrees.iter().any(|worktree| worktree.path == old_path) })
        );
        for paths in [
            &sidebar.pinned_worktrees,
            &sidebar.unread_worktrees,
            &sidebar.hidden_worktrees,
        ] {
            assert_eq!(paths.len(), 1);
            assert!(paths[0].matches(&old_path, None));
        }
    });
}

#[gpui::test]
async fn test_rename_same_path_worktree_uses_the_selected_repository(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let repository_a_root = PathBuf::from("/repository-a");
    let repository_b_root = PathBuf::from("/repository-b");
    let old_path = PathBuf::from("/worktrees/feature");
    let new_path = PathBuf::from("/worktrees/renamed");
    let repository_b_worktree_path = PathBuf::from("/worktrees/repository-b-feature");
    for root in [&repository_a_root, &repository_b_root] {
        fs.insert_tree(root, serde_json::json!({ ".git": {}, "src": {} }))
            .await;
    }
    let shared_worktree = git::repository::Worktree {
        path: old_path.clone(),
        ref_name: Some("refs/heads/feature".into()),
        sha: "abc".into(),
        is_main: false,
        is_bare: false,
    };
    let cached_worktree = shared_worktree.clone();
    fs.add_linked_worktree_for_repo(
        Path::new("/repository-a/.git"),
        false,
        shared_worktree.clone(),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/repository-b/.git"),
        false,
        git::repository::Worktree {
            path: repository_b_worktree_path.clone(),
            ..shared_worktree.clone()
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project = project::Project::test(
        fs.clone(),
        [repository_a_root.as_path(), repository_b_root.as_path()],
        cx,
    )
    .await;
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    let repositories = project.read_with(cx, |project, cx| {
        project
            .repositories(cx)
            .values()
            .map(|repository| {
                (
                    repository.read(cx).common_dir_abs_path.to_path_buf(),
                    repository.clone(),
                )
            })
            .collect::<HashMap<_, _>>()
    });
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    sidebar.update(cx, |sidebar, _| {
        for repository_key in repositories.keys() {
            sidebar.available_worktrees.insert(
                (repository_key.clone(), None),
                vec![cached_worktree.clone()],
            );
        }
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.start_renaming_worktree(
            old_path.clone(),
            None,
            Some(PathBuf::from("/repository-a/.git")),
            "feature".into(),
            window,
            cx,
        );
        sidebar.worktree_rename_editor.update(cx, |editor, cx| {
            editor.set_text("renamed", window, cx);
        });
        sidebar.commit_worktree_rename(window, cx);
        sidebar.start_renaming_worktree(
            old_path.clone(),
            None,
            Some(PathBuf::from("/repository-a/.git")),
            "feature".into(),
            window,
            cx,
        );
        sidebar.worktree_rename_editor.update(cx, |editor, cx| {
            editor.set_text("renamed", window, cx);
        });
        sidebar.commit_worktree_rename(window, cx);
    });
    cx.run_until_parked();

    assert!(fs.is_dir(&new_path).await);
    assert_eq!(
        PathBuf::from(
            fs.load(Path::new("/repository-a/.git/worktrees/feature/gitdir"))
                .await
                .expect("selected repository metadata should remain readable")
        ),
        new_path.join(".git")
    );
    assert_eq!(
        PathBuf::from(
            fs.load(Path::new("/repository-b/.git/worktrees/feature/gitdir"))
                .await
                .expect("other repository metadata should remain readable")
        ),
        repository_b_worktree_path.join(".git")
    );

    sidebar.read_with(cx, |sidebar, _| {
        assert!(sidebar.pending_worktree_renames.is_empty());
    });
}

#[gpui::test]
async fn test_rename_remote_linked_worktree_updates_open_project(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));

    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        workspace::init(app_state.clone(), cx);
        app_state
    });
    let server_fs = FakeFs::new(server_cx.executor());
    let main_path = PathBuf::from("/project");
    let old_path = PathBuf::from("/worktrees/project/feature/project");
    let new_path = PathBuf::from("/worktrees/project/feature/renamed");
    server_fs
        .insert_tree(&main_path, serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    server_fs
        .insert_tree(&old_path, serde_json::json!({ "src": {} }))
        .await;
    server_fs.set_branch_name(Path::new("/project/.git"), Some("main"));
    server_fs.insert_branches(Path::new("/project/.git"), &["main", "feature"]);
    server_fs
        .add_linked_worktree_for_repo(
            Path::new("/project/.git"),
            false,
            git::repository::Worktree {
                path: old_path.clone(),
                ref_name: Some("refs/heads/feature".into()),
                sha: "abc".into(),
                is_main: false,
                is_bare: false,
            },
        )
        .await;

    let (project, headless, remote_connection) =
        start_remote_project(&server_fs, &old_path, &app_state, None, cx, server_cx).await;
    let server_worktree = headless.read_with(server_cx, |headless, cx| {
        headless
            .worktree_store
            .read(cx)
            .worktrees()
            .next()
            .expect("server worktree should be open")
    });
    server_worktree.update(server_cx, |worktree, cx| {
        worktree
            .as_local_mut()
            .expect("server worktree should be local")
            .set_defer_watch(true, cx);
    });
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(app_state.fs.clone(), cx));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .next()
            .expect("remote worktree should be open")
            .read(cx)
            .id()
    });
    project.update(cx, |project, cx| {
        assert!(project.update_worktree_abs_path(
            worktree_id,
            Path::new("/stale-client-worktree-path"),
            cx,
        ));
    });
    project.read_with(cx, |project, cx| {
        assert_eq!(
            project
                .visible_worktrees(cx)
                .next()
                .expect("remote worktree should remain open")
                .read(cx)
                .abs_path()
                .as_ref(),
            Path::new("/stale-client-worktree-path"),
        );
    });
    let stale_key = ProjectGroupKey::new(
        Some(remote_connection.clone()),
        PathList::new(std::slice::from_ref(&old_path)),
    );
    multi_workspace.update(cx, |multi_workspace, _| {
        multi_workspace.test_add_project_group(workspace::ProjectGroup {
            key: stale_key.clone(),
            workspaces: Vec::new(),
            expanded: true,
        });
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.start_renaming_worktree(
            old_path.clone(),
            workspace_manager::host_cache_key(Some(&remote_connection)),
            Some(PathBuf::from("/project/.git")),
            "feature".into(),
            window,
            cx,
        );
        sidebar.worktree_rename_editor.update(cx, |editor, cx| {
            editor.set_text("renamed", window, cx);
        });
        sidebar.commit_worktree_rename(window, cx);
    });
    cx.run_until_parked();

    assert!(!server_fs.is_dir(&old_path).await);
    assert!(server_fs.is_dir(&new_path).await);
    project.read_with(cx, |project, cx| {
        let worktree_path = project
            .visible_worktrees(cx)
            .next()
            .expect("remote worktree should remain open")
            .read(cx)
            .abs_path();
        assert_eq!(worktree_path.as_ref(), new_path);
    });
    multi_workspace.read_with(cx, |multi_workspace, _| {
        assert!(!multi_workspace.project_group_keys().contains(&stale_key));
    });
}

#[gpui::test]
async fn test_cancel_force_delete_keeps_linked_worktree_workspace(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let worktree_root = PathBuf::from("/worktrees/project/feature/project");
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        &worktree_root,
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: worktree_root.clone(),
            ref_name: Some("refs/heads/feature".into()),
            sha: "abc".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    fs.with_git_state(Path::new("/project/.git"), true, |state| {
        state
            .worktrees_requiring_force_delete
            .insert(worktree_root.clone());
    })
    .unwrap();
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let linked_project = project::Project::test(fs.clone(), [worktree_root.as_path()], cx).await;
    for project in [&main_project, &linked_project] {
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
    }

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(linked_project, window, cx);
    });
    cx.run_until_parked();

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.delete_worktree(worktree_root.clone(), None, None, None, window, cx);
    });
    sidebar.read_with(cx, |sidebar, _| {
        assert!(
            sidebar
                .pending_worktree_deletions
                .contains(&workspace_manager::ScopedPath::new(
                    worktree_root.clone(),
                    None,
                ))
        );
    });
    cx.simulate_prompt_answer("Delete");
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    sidebar.read_with(cx, |sidebar, _| {
        assert!(sidebar.pending_worktree_deletions.is_empty());
    });

    assert!(fs.is_dir(&worktree_root).await);
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        2,
        "cancelling force delete must not close or persist removal of the workspace"
    );
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.delete_worktree(worktree_root.clone(), None, None, None, window, cx);
    });
    assert!(
        cx.has_pending_prompt(),
        "cancelling must release the in-flight guard so deletion can be retried"
    );
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
}

#[gpui::test]
async fn test_delete_prunable_worktree_force_cleans_git_metadata_and_sidebar_state(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let main_root = PathBuf::from("/project");
    let stale_root = PathBuf::from("/worktrees/stale");
    fs.insert_tree(&main_root, serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    fs.insert_tree(&stale_root, serde_json::json!({})).await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: stale_root.clone(),
            ref_name: Some("refs/heads/stale".into()),
            sha: "abc".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    fs.remove_file(
        &stale_root.join(".git"),
        fs::RemoveOptions {
            recursive: false,
            ignore_if_not_exists: false,
        },
    )
    .await
    .expect("test worktree should become prunable");
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project = project::Project::test(fs.clone(), [main_root.as_path()], cx).await;
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    let repository = project.read_with(cx, |project, cx| {
        project
            .repositories(cx)
            .values()
            .next()
            .expect("main repository should be available")
            .clone()
    });
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let stale_key = ProjectGroupKey::new(None, PathList::new(std::slice::from_ref(&stale_root)));
    multi_workspace.update(cx, |multi_workspace, _| {
        multi_workspace.test_add_project_group(workspace::ProjectGroup {
            key: stale_key.clone(),
            workspaces: Vec::new(),
            expanded: true,
        });
    });
    sidebar.update(cx, |sidebar, _| {
        sidebar.available_worktrees.insert(
            (PathBuf::from("/project/.git"), None),
            vec![git::repository::Worktree {
                path: stale_root.clone(),
                ref_name: Some("refs/heads/stale".into()),
                sha: "abc".into(),
                is_main: false,
                is_bare: false,
            }],
        );
        for paths in [
            &mut sidebar.pinned_worktrees,
            &mut sidebar.unread_worktrees,
            &mut sidebar.hidden_worktrees,
        ] {
            paths.push(workspace_manager::ScopedPath::new(stale_root.clone(), None));
        }
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.delete_worktree(
            stale_root.clone(),
            None,
            Some(stale_key.clone()),
            Some(PathBuf::from("/project/.git")),
            window,
            cx,
        );
        sidebar.delete_worktree(
            stale_root.clone(),
            None,
            Some(stale_key.clone()),
            Some(PathBuf::from("/project/.git")),
            window,
            cx,
        );
    });
    cx.simulate_prompt_answer("Delete");
    cx.run_until_parked();
    assert!(
        cx.has_pending_prompt(),
        "stale metadata should offer force cleanup"
    );
    assert!(
        cx.pending_prompt()
            .is_some_and(|(message, _)| message.contains("stale Git metadata")),
        "a duplicate click must not enqueue a second initial confirmation"
    );
    cx.simulate_prompt_answer("Force Delete");
    cx.run_until_parked();

    assert!(!fs.is_dir(&stale_root).await);
    let registered_worktrees = repository
        .update(cx, |repository, _| repository.worktrees())
        .await
        .expect("worktree listing should complete")
        .expect("worktrees should be listed");
    assert!(
        registered_worktrees
            .iter()
            .all(|worktree| worktree.path != stale_root),
        "forced cleanup must remove the stale Git registration"
    );
    multi_workspace.read_with(cx, |multi_workspace, _| {
        assert!(!multi_workspace.project_group_keys().contains(&stale_key));
    });
    sidebar.read_with(cx, |sidebar, _| {
        for paths in [
            &sidebar.pinned_worktrees,
            &sidebar.unread_worktrees,
            &sidebar.hidden_worktrees,
        ] {
            assert!(paths.is_empty());
        }
    });
}

#[gpui::test]
async fn test_delete_same_path_worktree_uses_the_selected_repository(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let repository_a_root = PathBuf::from("/repository-a");
    let repository_b_root = PathBuf::from("/repository-b");
    let stale_root = PathBuf::from("/worktrees/stale");
    for root in [&repository_a_root, &repository_b_root] {
        fs.insert_tree(root, serde_json::json!({ ".git": {}, "src": {} }))
            .await;
    }
    let stale_worktree = git::repository::Worktree {
        path: stale_root.clone(),
        ref_name: Some("refs/heads/stale".into()),
        sha: "abc".into(),
        is_main: false,
        is_bare: false,
    };
    for git_dir in [
        Path::new("/repository-a/.git"),
        Path::new("/repository-b/.git"),
    ] {
        fs.add_linked_worktree_for_repo(git_dir, false, stale_worktree.clone())
            .await;
    }
    fs.remove_file(
        &stale_root.join(".git"),
        fs::RemoveOptions {
            recursive: false,
            ignore_if_not_exists: false,
        },
    )
    .await
    .expect("test worktree should become prunable");
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project = project::Project::test(
        fs,
        [repository_a_root.as_path(), repository_b_root.as_path()],
        cx,
    )
    .await;
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    let repositories = project.read_with(cx, |project, cx| {
        project
            .repositories(cx)
            .values()
            .map(|repository| {
                (
                    repository.read(cx).common_dir_abs_path.to_path_buf(),
                    repository.clone(),
                )
            })
            .collect::<HashMap<_, _>>()
    });
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    sidebar.update(cx, |sidebar, _| {
        for repository_key in repositories.keys() {
            sidebar
                .available_worktrees
                .insert((repository_key.clone(), None), vec![stale_worktree.clone()]);
        }
    });
    let stale_key = ProjectGroupKey::new(None, PathList::new(std::slice::from_ref(&stale_root)));

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.delete_worktree(
            stale_root.clone(),
            None,
            Some(stale_key),
            Some(PathBuf::from("/repository-a/.git")),
            window,
            cx,
        );
    });
    cx.simulate_prompt_answer("Delete");
    cx.run_until_parked();
    cx.simulate_prompt_answer("Force Delete");
    cx.run_until_parked();

    let worktrees_a = repositories[Path::new("/repository-a/.git")]
        .update(cx, |repository, _| repository.worktrees())
        .await
        .expect("repository A listing should complete")
        .expect("repository A worktrees should be listed");
    let worktrees_b = repositories[Path::new("/repository-b/.git")]
        .update(cx, |repository, _| repository.worktrees())
        .await
        .expect("repository B listing should complete")
        .expect("repository B worktrees should be listed");
    assert!(
        worktrees_a
            .iter()
            .all(|worktree| worktree.path != stale_root)
    );
    assert!(
        worktrees_b
            .iter()
            .any(|worktree| worktree.path == stale_root)
    );
}

#[gpui::test]
async fn test_delete_without_repository_only_closes_missing_worktree(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    fs.insert_tree("/missing-worktree", serde_json::json!({}))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let stale_project =
        project::Project::test(fs.clone(), ["/missing-worktree".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(stale_project, window, cx);
    });
    cx.run_until_parked();

    submit_worktree_deletion(&sidebar, "/missing-worktree", cx);
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("OK");
    cx.run_until_parked();

    multi_workspace.read_with(cx, |multi_workspace, _| {
        assert_eq!(multi_workspace.workspaces().count(), 2);
        assert!(multi_workspace.project_group_keys().iter().any(|key| {
            key.path_list()
                .paths()
                .contains(&PathBuf::from("/missing-worktree"))
        }));
    });
    assert!(
        fs.metadata(Path::new("/missing-worktree"))
            .await
            .unwrap()
            .is_some()
    );

    fs.remove_dir(
        Path::new("/missing-worktree"),
        fs::RemoveOptions {
            recursive: true,
            ignore_if_not_exists: false,
        },
    )
    .await
    .unwrap();
    submit_worktree_deletion(&sidebar, "/missing-worktree", cx);

    multi_workspace.read_with(cx, |multi_workspace, _| {
        assert_eq!(multi_workspace.workspaces().count(), 1);
        assert!(multi_workspace.project_group_keys().iter().all(|key| {
            !key.path_list()
                .paths()
                .contains(&PathBuf::from("/missing-worktree"))
        }));
    });
}

#[gpui::test]
async fn test_delete_restored_worktree_without_loaded_workspace_removes_group(
    cx: &mut TestAppContext,
) {
    let project = init_test_project("/project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let stale_key =
        ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/missing-worktree")]));
    multi_workspace.update(cx, |multi_workspace, _| {
        multi_workspace.test_add_project_group(workspace::ProjectGroup {
            key: stale_key.clone(),
            workspaces: Vec::new(),
            expanded: true,
        });
    });
    sidebar.update(cx, |sidebar, _| {
        for paths in [
            &mut sidebar.pinned_worktrees,
            &mut sidebar.unread_worktrees,
            &mut sidebar.hidden_worktrees,
        ] {
            paths.push(workspace_manager::ScopedPath::new(
                PathBuf::from("/missing-worktree"),
                None,
            ));
        }
    });

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.delete_worktree(
            PathBuf::from("/missing-worktree"),
            None,
            Some(stale_key.clone()),
            None,
            window,
            cx,
        );
    });
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Delete");
    cx.run_until_parked();

    multi_workspace.read_with(cx, |multi_workspace, _| {
        assert!(!multi_workspace.project_group_keys().contains(&stale_key));
    });
    sidebar.read_with(cx, |sidebar, _| {
        for paths in [
            &sidebar.pinned_worktrees,
            &sidebar.unread_worktrees,
            &sidebar.hidden_worktrees,
        ] {
            assert!(paths.is_empty());
        }
    });
}

#[gpui::test]
async fn test_archive_selected_terminal_archives_closed_linked_worktree(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/worktrees/project/feature-a/project",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/worktrees/project/feature-a/project"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    agent_workspaces::test_support::record_zed_created_worktree(
        fs.as_ref(),
        Path::new("/worktrees/project/feature-a/project"),
        None,
        cx,
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    let terminal_id = TerminalId::new();
    workspace.update_in(cx, |workspace, window, cx| {
        terminal_view::terminal_panel::TerminalPanel::insert_test_center_terminal(
            workspace,
            terminal_id,
            window,
            cx,
        );
        terminal_view::terminal_panel::TerminalPanel::close_center_terminal(
            workspace,
            terminal_id,
            window,
            cx,
        );
    });
    let worktree_folder_paths =
        PathList::new(&[PathBuf::from("/worktrees/project/feature-a/project")]);
    let metadata = TerminalThreadMetadata {
        terminal_id,
        title: "Feature Terminal".into(),
        custom_title: None,
        created_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        worktree_paths: WorktreePaths::from_path_lists(
            PathList::new(&[PathBuf::from("/project")]),
            worktree_folder_paths.clone(),
        )
        .unwrap(),
        remote_connection: None,
        working_directory: None,
    };
    cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
            store.save(metadata, cx);
        });
    });
    sidebar.update(cx, |sidebar, cx| sidebar.update_entries(cx));
    cx.run_until_parked();

    let terminal_index = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id))
            .expect("terminal should be visible in sidebar")
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        match &sidebar.contents.entries[terminal_index] {
            ListEntry::Terminal(terminal) => match &terminal.workspace {
                ThreadEntryWorkspace::Closed { folder_paths, .. } => {
                    assert_eq!(folder_paths, &worktree_folder_paths);
                }
                ThreadEntryWorkspace::Open(_) => {
                    panic!("linked worktree terminal should start closed")
                }
            },
            _ => panic!("expected terminal row"),
        }
    });

    focus_sidebar(&sidebar, cx);
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(terminal_index);
    });
    cx.dispatch_action(ArchiveSelectedThread);
    for _ in 0..8 {
        cx.run_until_parked();
    }

    let terminal_metadata_deleted = cx.update(|_, cx| {
        TerminalThreadMetadataStore::global(cx)
            .read(cx)
            .entry(terminal_id)
            .is_none()
    });
    assert!(
        terminal_metadata_deleted,
        "terminal metadata should be deleted after closing from the sidebar"
    );
    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(&worktree_folder_paths, None, cx)
            })
            .is_none(),
        "temporary linked worktree workspace should be removed after archiving"
    );
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        1,
        "closing a closed linked worktree terminal should leave only the main workspace"
    );
    assert!(
        !fs.is_dir(Path::new("/worktrees/project/feature-a/project"))
            .await,
        "linked worktree directory should be removed from disk after closing its terminal"
    );
}

#[gpui::test]
async fn test_archive_selected_thread_closes_selected_agent_panel_terminal(
    cx: &mut TestAppContext,
) {
    let project = init_test_project_with_agent_panel("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    let terminal_id = insert_center_terminal(
        &workspace,
        &project,
        "Dev Server",
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );
    cx.run_until_parked();

    focus_sidebar(&sidebar, cx);
    let terminal_index = sidebar.read_with(cx, |sidebar, _cx| {
        sidebar
            .contents
            .entries
            .iter()
            .position(|entry| matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id))
            .expect("terminal should be visible in sidebar")
    });
    sidebar.update_in(cx, |sidebar, _window, _cx| {
        sidebar.selection = Some(terminal_index);
    });
    cx.dispatch_action(ArchiveSelectedThread);
    cx.run_until_parked();

    assert!(
        !center_has_terminal(&workspace, terminal_id, cx),
        "closing should remove the terminal from the centre pane"
    );
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(sidebar.contents.entries.iter().all(|entry| {
            !matches!(entry, ListEntry::Terminal(terminal) if terminal.metadata.terminal_id == terminal_id)
        }));
    });
    sidebar.read_with(cx, |_sidebar, cx| {
        let store = TerminalThreadMetadataStore::global(cx).read(cx);
        assert!(
            store.entry(terminal_id).is_none(),
            "terminal metadata should be deleted when closing from the sidebar"
        );
    });
}

fn type_in_search(sidebar: &Entity<Sidebar>, query: &str, cx: &mut gpui::VisualTestContext) {
    sidebar.update_in(cx, |sidebar, window, cx| {
        window.focus(&sidebar.filter_editor.focus_handle(cx), cx);
        sidebar.filter_editor.update(cx, |editor, cx| {
            editor.set_text(query, window, cx);
        });
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn test_click_clears_selection_and_focus_in_restores_it(cx: &mut TestAppContext) {
    let project = init_test_project("/my-project", cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_test_terminal(
        "Terminal A",
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 2, 0, 0, 0).unwrap(),
        &project,
        cx,
    );

    save_test_terminal(
        "Terminal B",
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        &project,
        cx,
    );

    cx.run_until_parked();
    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [my-project]",
            "  Terminal A",
            "  Terminal B",
        ]
    );

    // Keyboard confirm preserves selection.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.selection = Some(1);
        sidebar.confirm(&Confirm, window, cx);
    });
    assert_eq!(
        sidebar.read_with(cx, |sidebar, _| sidebar.selection),
        Some(1)
    );

    // Click handlers clear selection to None so no highlight lingers
    // after a click regardless of focus state. The hover style provides
    // visual feedback during mouse interaction instead.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.selection = None;
        let path_list = PathList::new(&[std::path::PathBuf::from("/my-project")]);
        let project_group_key = ProjectGroupKey::new(None, path_list);
        sidebar.toggle_collapse(&project_group_key, window, cx);
    });
    assert_eq!(sidebar.read_with(cx, |sidebar, _| sidebar.selection), None);

    // When the user tabs back into the sidebar, focus_in no longer
    // restores selection — it stays None.
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.focus_in(window, cx);
    });
    assert_eq!(sidebar.read_with(cx, |sidebar, _| sidebar.selection), None);
}

async fn init_test_project_with_git(
    worktree_path: &str,
    cx: &mut TestAppContext,
) -> (Entity<project::Project>, Arc<dyn fs::Fs>) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        worktree_path,
        serde_json::json!({
            ".git": {},
            "src": {},
        }),
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = project::Project::test(fs.clone(), [worktree_path.as_ref()], cx).await;
    (project, fs)
}

#[gpui::test]
async fn test_git_worktree_added_live_updates_sidebar(cx: &mut TestAppContext) {
    let (project, fs) = init_test_project_with_git("/project", cx).await;

    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let worktree_project = project::Project::test(fs.clone(), ["/wt/rosewood".as_ref()], cx).await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    // Save a thread against a worktree path with the correct main
    // worktree association (as if the git state had been resolved).
    save_terminal_metadata_with_main_paths(
        "Worktree Terminal",
        PathList::new(&[PathBuf::from("/wt/rosewood")]),
        PathList::new(&[PathBuf::from("/project")]),
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        cx,
    );

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Thread is visible because its main_worktree_paths match the group.
    // The chip name is derived from the path even before git discovery.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec!["v [project]", "  Worktree Terminal {rosewood}"]
    );

    // Now add the worktree to the git state and trigger a rescan.
    fs.as_fake()
        .add_linked_worktree_for_repo(
            Path::new("/project/.git"),
            true,
            git::repository::Worktree {
                path: std::path::PathBuf::from("/wt/rosewood"),
                ref_name: Some("refs/heads/rosewood".into()),
                sha: "abc".into(),
                is_main: false,
                is_bare: false,
            },
        )
        .await;

    cx.run_until_parked();

    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project]",
            "  Worktree Terminal {rosewood}",
        ]
    );
}

#[gpui::test]
async fn test_two_worktree_workspaces_absorbed_when_main_added(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    // Create the main repo directory (not opened as a workspace yet).
    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
            },
            "src": {},
        }),
    )
    .await;

    // Two worktree checkouts whose .git files point back to the main repo.
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "aaa".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: std::path::PathBuf::from("/wt-feature-b"),
            ref_name: Some("refs/heads/feature-b".into()),
            sha: "bbb".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;

    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let project_a = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    let project_b = project::Project::test(fs.clone(), ["/wt-feature-b".as_ref()], cx).await;

    project_a.update(cx, |p, cx| p.git_scans_complete(cx)).await;
    project_b.update(cx, |p, cx| p.git_scans_complete(cx)).await;

    // Open both worktrees as workspaces — no main repo yet.
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a.clone(), window, cx));
    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project_b.clone(), window, cx);
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);

    save_test_terminal(
        "Terminal A",
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
        &project_a,
        cx,
    );
    save_test_terminal(
        "Terminal B",
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 1).unwrap(),
        &project_b,
        cx,
    );

    multi_workspace.update_in(cx, |_, _window, cx| cx.notify());
    cx.run_until_parked();

    // Without the main repo, each worktree has its own header.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project]",
            "  Terminal B {wt-feature-b}",
            "  Terminal A {wt-feature-a}",
        ]
    );

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(main_project.clone(), window, cx);
    });
    cx.run_until_parked();

    // Both worktree workspaces should now be absorbed under the main
    // repo header, with worktree chips.
    assert_eq!(
        visible_entries_as_strings(&sidebar, cx),
        vec![
            //
            "v [project]",
            "  Terminal B {wt-feature-b}",
            "  Terminal A {wt-feature-a}",
        ]
    );
}

#[gpui::test]
async fn test_restore_worktree_when_branch_has_moved(cx: &mut TestAppContext) {
    // restore_worktree_via_git should succeed when the branch has moved
    // to a different SHA since archival. The worktree stays in detached
    // HEAD and the moved branch is left untouched.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-a": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-a",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/wt-feature-a",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-a",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/wt-feature-a"),
            ref_name: Some("refs/heads/feature-a".into()),
            sha: "original-sha".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-a".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, _cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    multi_workspace.update_in(_cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    let wt_repo = worktree_project.read_with(cx, |project, cx| {
        project.repositories(cx).values().next().unwrap().clone()
    });
    let (staged_hash, unstaged_hash) = cx
        .update(|cx| wt_repo.update(cx, |repo, _| repo.create_archive_checkpoint()))
        .await
        .unwrap()
        .unwrap();

    // Move the branch to a different SHA.
    fs.with_git_state(Path::new("/project/.git"), false, |state| {
        state
            .refs
            .insert("refs/heads/feature-a".into(), "moved-sha".into());
    })
    .unwrap();

    let result = cx
        .spawn(|mut cx| async move {
            agent_workspaces::thread_worktree_archive::restore_worktree_via_git(
                &agent_workspaces::ArchivedGitWorktree {
                    id: 1,
                    worktree_path: PathBuf::from("/wt-feature-a"),
                    main_repo_path: PathBuf::from("/project"),
                    branch_name: Some("feature-a".to_string()),
                    staged_commit_hash: staged_hash,
                    unstaged_commit_hash: unstaged_hash,
                    original_commit_hash: "original-sha".to_string(),
                },
                None,
                &mut cx,
            )
            .await
        })
        .await;

    assert!(
        result.is_ok(),
        "restore should succeed even when branch has moved: {:?}",
        result.err()
    );

    // The moved branch ref should be completely untouched.
    let branch_sha = fs
        .with_git_state(Path::new("/project/.git"), false, |state| {
            state.refs.get("refs/heads/feature-a").cloned()
        })
        .unwrap();
    assert_eq!(
        branch_sha.as_deref(),
        Some("moved-sha"),
        "the moved branch ref should not be modified by the restore"
    );
}

#[gpui::test]
async fn test_restore_worktree_when_branch_has_not_moved(cx: &mut TestAppContext) {
    // restore_worktree_via_git should succeed when the branch still
    // points at the same SHA as at archive time.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-b": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-b",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/wt-feature-b",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-b",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/wt-feature-b"),
            ref_name: Some("refs/heads/feature-b".into()),
            sha: "original-sha".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-b".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, _cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    multi_workspace.update_in(_cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    let wt_repo = worktree_project.read_with(cx, |project, cx| {
        project.repositories(cx).values().next().unwrap().clone()
    });
    let (staged_hash, unstaged_hash) = cx
        .update(|cx| wt_repo.update(cx, |repo, _| repo.create_archive_checkpoint()))
        .await
        .unwrap()
        .unwrap();

    // refs/heads/feature-b already points at "original-sha" (set by
    // add_linked_worktree_for_repo), matching original_commit_hash.

    let result = cx
        .spawn(|mut cx| async move {
            agent_workspaces::thread_worktree_archive::restore_worktree_via_git(
                &agent_workspaces::ArchivedGitWorktree {
                    id: 1,
                    worktree_path: PathBuf::from("/wt-feature-b"),
                    main_repo_path: PathBuf::from("/project"),
                    branch_name: Some("feature-b".to_string()),
                    staged_commit_hash: staged_hash,
                    unstaged_commit_hash: unstaged_hash,
                    original_commit_hash: "original-sha".to_string(),
                },
                None,
                &mut cx,
            )
            .await
        })
        .await;

    assert!(
        result.is_ok(),
        "restore should succeed when branch has not moved: {:?}",
        result.err()
    );
}

#[gpui::test]
async fn test_restore_worktree_when_branch_does_not_exist(cx: &mut TestAppContext) {
    // restore_worktree_via_git should succeed when the branch no longer
    // exists (e.g. it was deleted while the thread was archived). The
    // code should attempt to recreate the branch.
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree(
        "/project",
        serde_json::json!({
            ".git": {
                "worktrees": {
                    "feature-d": {
                        "commondir": "../../",
                        "HEAD": "ref: refs/heads/feature-d",
                    },
                },
            },
            "src": {},
        }),
    )
    .await;
    fs.insert_tree(
        "/wt-feature-d",
        serde_json::json!({
            ".git": "gitdir: /project/.git/worktrees/feature-d",
            "src": {},
        }),
    )
    .await;
    fs.add_linked_worktree_for_repo(
        Path::new("/project/.git"),
        false,
        git::repository::Worktree {
            path: PathBuf::from("/wt-feature-d"),
            ref_name: Some("refs/heads/feature-d".into()),
            sha: "original-sha".into(),
            is_main: false,
            is_bare: false,
        },
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));

    let main_project = project::Project::test(fs.clone(), ["/project".as_ref()], cx).await;
    let worktree_project = project::Project::test(fs.clone(), ["/wt-feature-d".as_ref()], cx).await;
    main_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;
    worktree_project
        .update(cx, |p, cx| p.git_scans_complete(cx))
        .await;

    let (multi_workspace, _cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project.clone(), window, cx));
    multi_workspace.update_in(_cx, |mw, window, cx| {
        mw.test_add_workspace(worktree_project.clone(), window, cx)
    });

    let wt_repo = worktree_project.read_with(cx, |project, cx| {
        project.repositories(cx).values().next().unwrap().clone()
    });
    let (staged_hash, unstaged_hash) = cx
        .update(|cx| wt_repo.update(cx, |repo, _| repo.create_archive_checkpoint()))
        .await
        .unwrap()
        .unwrap();

    // Remove the branch ref so change_branch will fail.
    fs.with_git_state(Path::new("/project/.git"), false, |state| {
        state.refs.remove("refs/heads/feature-d");
    })
    .unwrap();

    let result = cx
        .spawn(|mut cx| async move {
            agent_workspaces::thread_worktree_archive::restore_worktree_via_git(
                &agent_workspaces::ArchivedGitWorktree {
                    id: 1,
                    worktree_path: PathBuf::from("/wt-feature-d"),
                    main_repo_path: PathBuf::from("/project"),
                    branch_name: Some("feature-d".to_string()),
                    staged_commit_hash: staged_hash,
                    unstaged_commit_hash: unstaged_hash,
                    original_commit_hash: "original-sha".to_string(),
                },
                None,
                &mut cx,
            )
            .await
        })
        .await;

    assert!(
        result.is_ok(),
        "restore should succeed when branch does not exist: {:?}",
        result.err()
    );
}

async fn init_multi_project_test(
    paths: &[&str],
    cx: &mut TestAppContext,
) -> (Arc<FakeFs>, Entity<project::Project>) {
    agent_workspaces::test_support::init_test(cx);
    let fs = FakeFs::new(cx.executor());
    for path in paths {
        fs.insert_tree(path, serde_json::json!({ ".git": {}, "src": {} }))
            .await;
    }
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project =
        project::Project::test(fs.clone() as Arc<dyn fs::Fs>, [paths[0].as_ref()], cx).await;
    (fs, project)
}

async fn add_test_project(
    path: &str,
    fs: &Arc<FakeFs>,
    multi_workspace: &Entity<MultiWorkspace>,
    cx: &mut gpui::VisualTestContext,
) -> Entity<Workspace> {
    let project = project::Project::test(fs.clone() as Arc<dyn fs::Fs>, [path.as_ref()], cx).await;
    let workspace = multi_workspace.update_in(cx, |mw, window, cx| {
        mw.test_add_workspace(project, window, cx)
    });
    cx.run_until_parked();
    workspace
}

async fn init_sidebar_create_worktree_test(
    cx: &mut TestAppContext,
) -> (Arc<FakeFs>, Entity<project::Project>) {
    agent_workspaces::test_support::init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project-a",
        serde_json::json!({
            ".git": {},
            "src": { "main.rs": "fn main() {}" },
        }),
    )
    .await;
    fs.insert_tree("/unrelated", serde_json::json!({ "src": {} }))
        .await;
    fs.set_head_for_repo(
        Path::new("/project-a/.git"),
        &[("src/main.rs", "fn main() {}".to_string())],
        "deadbeef",
    );
    fs.set_branch_name(Path::new("/project-a/.git"), Some("main"));
    fs.insert_branches(Path::new("/project-a/.git"), &["main"]);
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project =
        project::Project::test(fs.clone() as Arc<dyn fs::Fs>, [Path::new("/project-a")], cx).await;
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    (fs, project)
}

#[gpui::test]
async fn test_worktree_picker_activates_its_project(cx: &mut TestAppContext) {
    let (fs, project) = init_sidebar_create_worktree_test(cx).await;
    let initial_paths = project.read_with(cx, |project, cx| project.worktree_paths(cx));
    let unrelated_project = project::Project::test(fs, [Path::new("/unrelated")], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let unrelated_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(unrelated_project, window, cx)
    });
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone()),
        unrelated_workspace,
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.open_worktree_picker(&workspace, window, cx);
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone()),
        workspace,
        "the picker must be mounted on the project it will create from",
    );
    assert!(
        workspace
            .read_with(cx, |workspace, cx| workspace
                .active_modal::<git_ui::worktree_picker::WorktreePicker>(
                cx
            ))
            .is_some(),
        "the worktree picker should be visible on the activated project",
    );
    assert_eq!(
        workspace.read_with(cx, |workspace, cx| workspace
            .project()
            .read(cx)
            .worktree_paths(cx)),
        initial_paths,
        "opening the picker must not create a worktree immediately",
    );
}

#[gpui::test]
async fn test_created_worktree_clears_source_sidebar_selection(cx: &mut TestAppContext) {
    let (_fs, project) = init_sidebar_create_worktree_test(cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let source_workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let source_row = sidebar.update(cx, |sidebar, cx| {
        let tree = sidebar.workspace_tree(cx);
        tree.rows()
            .iter()
            .position(|row| {
                let workspace_manager::RowKind::Worktree(id) = row.kind else {
                    return false;
                };
                tree.workspace_for(id)
                    .is_some_and(|workspace| workspace.entity_id() == source_workspace.entity_id())
            })
            .expect("the source workspace should have a worktree row")
    });
    sidebar.update(cx, |sidebar, _| sidebar.selection = Some(source_row));
    assert_eq!(
        sidebar.read_with(cx, |sidebar, _| sidebar.selection),
        Some(source_row),
        "the source row should start selected"
    );

    source_workspace.update_in(cx, |workspace, window, cx| {
        git_ui::worktree_service::handle_create_worktree(
            workspace,
            &CreateWorktree {
                worktree_name: Some("feature".to_string()),
                branch_target: NewWorktreeBranchTarget::CurrentBranch,
            },
            window,
            None,
            cx,
        );
    });
    cx.run_until_parked();

    assert_ne!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone()),
        source_workspace,
        "the created worktree should be active"
    );
    assert_eq!(
        sidebar.read_with(cx, |sidebar, _| sidebar.selection),
        None,
        "the source row must not stay highlighted after the new worktree activates"
    );
}

fn capture_center_terminal_requests(
    cx: &mut gpui::VisualTestContext,
) -> Arc<Mutex<Vec<WorktreePaths>>> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    cx.update(|_, cx| {
        terminal_view::terminal_panel::on_add_center_terminal(cx, {
            let requests = requests.clone();
            move |workspace, _, _, cx| {
                requests
                    .lock()
                    .expect("terminal request mutex should not be poisoned")
                    .push(workspace.project().read(cx).worktree_paths(cx));
                Some(Task::ready(Err(anyhow::anyhow!(
                    "captured center terminal request"
                ))))
            }
        });
    });
    requests
}

#[gpui::test]
async fn test_claimed_ssh_workspace_does_not_open_a_stock_terminal(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        workspace::init(app_state.clone(), cx);
        app_state
    });
    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree("/project", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    let (project, _headless, _) = start_remote_project(
        &server_fs,
        Path::new("/project"),
        &app_state,
        None,
        cx,
        server_cx,
    )
    .await;
    let remote_client = project
        .read_with(cx, |project, _| project.remote_client())
        .expect("remote test project should have a client");
    remote_client.update(cx, |remote_client, _| {
        remote_client.test_set_connection_options(remote::RemoteConnectionOptions::Ssh(
            remote::SshConnectionOptions {
                host: "test-host".into(),
                username: Some("user".to_owned()),
                ..Default::default()
            },
        ));
    });

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar =
        cx.update(|window, cx| cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx)));
    multi_workspace.update(cx, |multi_workspace, cx| {
        multi_workspace.register_sidebar(sidebar.clone(), cx);
    });
    let terminal_requests = capture_center_terminal_requests(cx);
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_new_terminal(&workspace, window, cx);
    });
    assert!(
        terminal_requests
            .lock()
            .expect("terminal request mutex should not be poisoned")
            .is_empty(),
        "the connection flow owns the first terminal while ADE attachment is in flight",
    );

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.set_ade_owns_layout(window, cx);
    });
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_new_terminal(&workspace, window, cx);
    });
    assert_eq!(
        terminal_requests
            .lock()
            .expect("terminal request mutex should not be poisoned")
            .len(),
        1,
        "a later click in an ADE-owned empty workspace should create one terminal",
    );
}

#[gpui::test(iterations = 10)]
async fn test_created_worktree_gets_terminal_when_an_unrelated_workspace_add_races(
    cx: &mut TestAppContext,
) {
    let (fs, project) = init_sidebar_create_worktree_test(cx).await;
    let unrelated_project = project::Project::test(fs, [Path::new("/unrelated")], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let terminal_requests = capture_center_terminal_requests(cx);
    let source_workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    let source_path = source_workspace.read_with(cx, |workspace, cx| {
        workspace
            .project()
            .read(cx)
            .find_project_path("/project-a/src/main.rs", cx)
            .expect("source file should be in the project")
    });
    source_workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(source_path, None, true, window, cx)
        })
        .await
        .expect("source file should open");

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_worktree(
            &source_workspace,
            Some("feature".to_string()),
            NewWorktreeBranchTarget::CurrentBranch,
            window,
            cx,
        );
    });
    let unrelated_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(unrelated_project, window, cx)
    });
    cx.run_until_parked();

    let created_workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    assert_ne!(created_workspace, source_workspace);
    assert_ne!(created_workspace, unrelated_workspace);
    let created_paths = created_workspace.read_with(cx, |workspace, cx| {
        workspace.project().read(cx).worktree_paths(cx)
    });
    assert_eq!(
        *terminal_requests
            .lock()
            .expect("terminal request mutex should not be poisoned"),
        [created_paths],
        "only the exact created workspace should receive the terminal request",
    );
    assert_eq!(
        source_workspace.read_with(cx, |workspace, cx| workspace.open_item_abs_paths(cx)),
        [PathBuf::from("/project-a/src/main.rs")],
        "creating a worktree must not disturb the source workspace",
    );
    assert!(
        created_workspace
            .read_with(cx, |workspace, cx| workspace.open_item_abs_paths(cx))
            .is_empty(),
        "the created worktree must not inherit source tabs",
    );
    sidebar.read_with(cx, |sidebar, _| {
        assert_eq!(sidebar.selection, None);
    });
}

#[gpui::test(iterations = 10)]
async fn test_failed_worktree_creation_does_not_affect_the_next_workspace(cx: &mut TestAppContext) {
    let (fs, project) = init_sidebar_create_worktree_test(cx).await;
    fs.set_create_worktree_error(
        Path::new("/project-a/.git"),
        Some("simulated worktree creation failure".to_string()),
    );
    let unrelated_project = project::Project::test(fs, [Path::new("/unrelated")], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let terminal_requests = capture_center_terminal_requests(cx);
    let source_workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.create_worktree(
            &source_workspace,
            Some("feature".to_string()),
            NewWorktreeBranchTarget::CurrentBranch,
            window,
            cx,
        );
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone()),
        source_workspace,
    );
    assert!(
        terminal_requests
            .lock()
            .expect("terminal request mutex should not be poisoned")
            .is_empty()
    );
    let unrelated_workspace = multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(unrelated_project, window, cx)
    });
    cx.run_until_parked();

    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone()),
        unrelated_workspace,
    );
    assert!(
        terminal_requests
            .lock()
            .expect("terminal request mutex should not be poisoned")
            .is_empty(),
        "a failed worktree creation must not arm the next workspace for a terminal",
    );
    assert_eq!(
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace
            .workspaces()
            .count()),
        2,
    );
}

#[gpui::test]
async fn test_workspace_lifecycle_retains_projects_when_sidebar_is_closed(cx: &mut TestAppContext) {
    let (fs, project_a) =
        init_multi_project_test(&["/project-a", "/project-b", "/project-c"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let _sidebar = setup_sidebar_closed(&multi_workspace, cx);

    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    assert!(!multi_workspace.read_with(cx, |mw, _| mw.sidebar_open()));
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        1
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_a));

    let workspace_b = add_test_project("/project-b", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_b));
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_a)));

    let workspace_c = add_test_project("/project-c", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        3
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_c));
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_b)));
}

#[gpui::test]
async fn test_workspaces_remain_retained_after_sidebar_closes(cx: &mut TestAppContext) {
    let (fs, project_a) = init_multi_project_test(
        &["/project-a", "/project-b", "/project-c", "/project-d"],
        cx,
    )
    .await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);
    assert!(multi_workspace.read_with(cx, |mw, _| mw.sidebar_open()));
    let workspace_a = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let workspace_b = add_test_project("/project-b", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );

    multi_workspace.update_in(cx, |mw, window, cx| {
        mw.activate(workspace_a, None, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_b)));

    multi_workspace.update_in(cx, |mw, window, cx| mw.close_sidebar(window, cx));
    cx.run_until_parked();
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        2
    );

    let workspace_c = add_test_project("/project-c", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        3
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_c));

    let workspace_d = add_test_project("/project-d", &fs, &multi_workspace, cx).await;
    assert_eq!(
        multi_workspace.read_with(cx, |mw, _| mw.workspaces().count()),
        4
    );
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspace() == &workspace_d));
    assert!(multi_workspace.read_with(cx, |mw, _| mw.workspaces().any(|w| w == &workspace_c)));
}

#[test]
fn test_worktree_info_branch_names_for_main_worktrees() {
    let folder_paths = PathList::new(&[PathBuf::from("/projects/myapp")]);
    let worktree_paths = WorktreePaths::from_folder_paths(&folder_paths);

    let branch_by_path: HashMap<PathBuf, SharedString> =
        [(PathBuf::from("/projects/myapp"), "feature-x".into())]
            .into_iter()
            .collect();

    let infos = worktree_info_from_thread_paths(&worktree_paths, &branch_by_path);
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].kind, ui::WorktreeKind::Main);
    assert_eq!(infos[0].branch_name, Some(SharedString::from("feature-x")));
    assert_eq!(infos[0].worktree_name, Some(SharedString::from("myapp")));
}

#[test]
fn test_worktree_info_branch_names_for_linked_worktrees() {
    let main_paths = PathList::new(&[PathBuf::from("/projects/myapp")]);
    let folder_paths = PathList::new(&[PathBuf::from("/projects/myapp-feature")]);
    let worktree_paths =
        WorktreePaths::from_path_lists(main_paths, folder_paths).expect("same length");

    let branch_by_path: HashMap<PathBuf, SharedString> = [(
        PathBuf::from("/projects/myapp-feature"),
        "feature-branch".into(),
    )]
    .into_iter()
    .collect();

    let infos = worktree_info_from_thread_paths(&worktree_paths, &branch_by_path);
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].kind, ui::WorktreeKind::Linked);
    assert_eq!(
        infos[0].branch_name,
        Some(SharedString::from("feature-branch"))
    );
}

#[test]
fn test_worktree_info_missing_branch_returns_none() {
    let folder_paths = PathList::new(&[PathBuf::from("/projects/myapp")]);
    let worktree_paths = WorktreePaths::from_folder_paths(&folder_paths);

    let branch_by_path: HashMap<PathBuf, SharedString> = HashMap::new();

    let infos = worktree_info_from_thread_paths(&worktree_paths, &branch_by_path);
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].kind, ui::WorktreeKind::Main);
    assert_eq!(infos[0].branch_name, None);
    assert_eq!(infos[0].worktree_name, Some(SharedString::from("myapp")));
}

#[gpui::test]
async fn test_disconnected_remote_worktree_deletion_does_not_stay_pending(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));

    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        workspace::init(app_state.clone(), cx);
        app_state
    });
    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree("/project", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    server_fs
        .insert_tree(
            "/external-worktree",
            serde_json::json!({
                ".git": "gitdir: /project/.git/worktrees/feature-a",
                "untracked.txt": "keep me",
            }),
        )
        .await;
    server_fs.set_branch_name(Path::new("/project/.git"), Some("main"));
    server_fs.insert_branches(Path::new("/project/.git"), &["main", "feature-a"]);
    server_fs
        .add_linked_worktree_for_repo(
            Path::new("/project/.git"),
            false,
            git::repository::Worktree {
                path: PathBuf::from("/external-worktree"),
                ref_name: Some("refs/heads/feature-a".into()),
                sha: "abc".into(),
                is_main: false,
                is_bare: false,
            },
        )
        .await;
    server_fs
        .with_git_state(Path::new("/project/.git"), false, |state| {
            state
                .worktrees_requiring_force_delete
                .insert(PathBuf::from("/external-worktree"));
        })
        .expect("remote worktree should be marked dirty");

    let (project, _headless, _remote_connection) = start_remote_project(
        &server_fs,
        Path::new("/external-worktree"),
        &app_state,
        None,
        cx,
        server_cx,
    )
    .await;
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(app_state.fs.clone(), cx));

    let remote_client = project.read_with(cx, |project, _| {
        project
            .remote_client()
            .expect("remote project should have a client")
    });
    let remote_connection = remote_client.read_with(cx, |remote_client, _| {
        remote_client
            .connection()
            .expect("remote client should be connected before the simulated failure")
    });
    remote_client.update(cx, |remote_client, cx| {
        remote_client.force_server_not_running(cx);
    });
    remote_connection.simulate_disconnect(&cx.to_async());
    cx.run_until_parked();
    project.read_with(cx, |project, cx| assert!(project.is_disconnected(cx)));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.delete_worktree(
            PathBuf::from("/external-worktree"),
            None,
            None,
            None,
            window,
            cx,
        );
    });
    cx.simulate_prompt_answer("Delete");
    cx.run_until_parked();

    sidebar.read_with(cx, |sidebar, _| {
        assert!(sidebar.pending_worktree_deletions.is_empty());
    });
    assert!(
        server_fs
            .is_file(Path::new("/external-worktree/untracked.txt"))
            .await
    );
}

#[gpui::test]
async fn test_remote_linked_worktree_deletion_uses_remote_connection(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);

    cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });
    server_cx.update(|cx| {
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });

    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        workspace::init(app_state.clone(), cx);
        app_state
    });

    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree(
            "/project",
            serde_json::json!({
                ".git": {},
                "src": {},
            }),
        )
        .await;
    server_fs
        .insert_tree(
            "/external-worktree",
            serde_json::json!({
                ".git": "gitdir: /project/.git/worktrees/feature-a",
                "src": {},
            }),
        )
        .await;
    server_fs.set_branch_name(Path::new("/project/.git"), Some("main"));
    server_fs.insert_branches(Path::new("/project/.git"), &["main", "feature-a"]);
    server_fs
        .add_linked_worktree_for_repo(
            Path::new("/project/.git"),
            false,
            git::repository::Worktree {
                path: PathBuf::from("/external-worktree"),
                ref_name: Some("refs/heads/feature-a".into()),
                sha: "abc".into(),
                is_main: false,
                is_bare: false,
            },
        )
        .await;

    let (worktree_project, _headless, remote_connection) = start_remote_project(
        &server_fs,
        Path::new("/external-worktree"),
        &app_state,
        None,
        cx,
        server_cx,
    )
    .await;
    worktree_project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    cx.run_until_parked();

    cx.update(|cx| <dyn fs::Fs>::set_global(app_state.fs.clone(), cx));

    let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
        MultiWorkspace::test_new(worktree_project.clone(), window, cx)
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_folder_paths = PathList::new(&[PathBuf::from("/external-worktree")]);
    let main_folder_paths = PathList::new(&[PathBuf::from("/project")]);
    let worktree_terminal_id = TerminalId::new();
    cx.update(|_window, cx| {
        let metadata = TerminalThreadMetadata {
            terminal_id: worktree_terminal_id,
            title: "Remote Worktree Terminal".into(),
            custom_title: None,
            created_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2024, 1, 1, 0, 0, 0).unwrap(),
            worktree_paths: WorktreePaths::from_path_lists(
                main_folder_paths,
                worktree_folder_paths.clone(),
            )
            .unwrap(),
            remote_connection: Some(remote_connection.clone()),
            working_directory: None,
        };
        TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| store.save(metadata, cx));
    });
    cx.run_until_parked();

    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(
                    &worktree_folder_paths,
                    Some(&remote_connection),
                    cx,
                )
            })
            .is_some(),
        "remote linked-worktree workspace should be open before archiving"
    );
    assert!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace_for_paths(&worktree_folder_paths, None, cx)
            })
            .is_none(),
        "the test must exercise a remote-only workspace lookup"
    );
    assert_ne!(
        multi_workspace
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace.workspace().read(cx).project_group_key(cx)
            })
            .path_list(),
        &worktree_folder_paths,
        "remote workspace must be classified as a linked worktree under the main project"
    );

    let worktree_id = worktree_project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .next()
            .expect("remote worktree should exist")
            .read(cx)
            .id()
    });
    worktree_project.update(cx, |project, cx| {
        assert!(project.update_worktree_abs_path(worktree_id, Path::new("/renamed-worktree"), cx,));
        assert_eq!(
            project.project_group_key(cx).path_list(),
            &PathList::new(&[PathBuf::from("/project")]),
            "renaming a remote checkout must preserve its main-repository identity",
        );
        project.update_worktree_abs_path(worktree_id, Path::new("/external-worktree"), cx);
    });

    let workspace_to_remove = sidebar.read_with(cx, |sidebar, cx| {
        sidebar
            .linked_worktree_workspace_to_remove(
                &worktree_folder_paths,
                Some(&remote_connection),
                Some(worktree_terminal_id),
                &[],
                cx,
            )
            .map(|workspace| workspace.entity_id())
    });
    let active_workspace_id = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().entity_id()
    });
    assert_eq!(
        workspace_to_remove,
        Some(active_workspace_id),
        "archive helper should resolve the remote linked-worktree workspace"
    );
    assert!(
        server_fs.is_dir(Path::new("/external-worktree")).await,
        "direct helper check should not remove the linked worktree from disk"
    );

    let phantom_key = ProjectGroupKey::new(
        Some(remote_connection.clone()),
        worktree_folder_paths.clone(),
    );
    multi_workspace.update(cx, |multi_workspace, _| {
        multi_workspace.test_add_project_group(workspace::ProjectGroup {
            key: phantom_key.clone(),
            workspaces: Vec::new(),
            expanded: true,
        });
    });

    submit_worktree_deletion(&sidebar, "/external-worktree", cx);
    assert!(
        !server_fs.is_dir(Path::new("/external-worktree")).await,
        "remote worktree deletion must run on the remote filesystem"
    );
    multi_workspace.read_with(cx, |multi_workspace, _| {
        assert_eq!(multi_workspace.workspaces().count(), 1);
        assert!(!multi_workspace.project_group_keys().contains(&phantom_key));
    });
}

#[gpui::test]
async fn test_remote_git_worktree_lifecycle(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));

    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        workspace::init(app_state.clone(), cx);
        app_state
    });
    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree("/project", serde_json::json!({ ".git": {}, "src": {} }))
        .await;
    server_fs.set_branch_name(Path::new("/project/.git"), Some("main"));
    server_fs.insert_branches(Path::new("/project/.git"), &["main"]);

    let (project, _headless, _remote_connection) = start_remote_project(
        &server_fs,
        Path::new("/project"),
        &app_state,
        None,
        cx,
        server_cx,
    )
    .await;
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let repository = project.read_with(cx, |project, cx| {
        project.repositories(cx).values().next().unwrap().clone()
    });
    let list = |cx: &mut TestAppContext| {
        cx.update(|cx| repository.update(cx, |repository, _| repository.worktrees()))
    };

    assert_eq!(list(cx).await.unwrap().unwrap().len(), 1);

    let original_path = PathBuf::from("/worktrees/feature");
    cx.update(|cx| {
        repository.update(cx, |repository, _| {
            repository.create_worktree(
                git::repository::CreateWorktreeTarget::NewBranch {
                    branch_name: "feature".to_string(),
                    base_sha: None,
                },
                original_path.clone(),
            )
        })
    })
    .await
    .unwrap()
    .unwrap();
    assert!(
        list(cx)
            .await
            .unwrap()
            .unwrap()
            .iter()
            .any(|worktree| worktree.path == original_path)
    );

    let renamed_path = PathBuf::from("/worktrees/renamed");
    cx.update(|cx| {
        repository.update(cx, |repository, _| {
            repository.rename_worktree(original_path.clone(), renamed_path.clone())
        })
    })
    .await
    .unwrap()
    .unwrap();
    assert!(
        list(cx)
            .await
            .unwrap()
            .unwrap()
            .iter()
            .any(|worktree| worktree.path == renamed_path)
    );

    cx.update(|cx| {
        repository.update(cx, |repository, _| {
            repository.remove_worktree(renamed_path.clone(), false)
        })
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(list(cx).await.unwrap().unwrap().len(), 1);
    assert!(!server_fs.is_dir(&renamed_path).await);
}

#[gpui::test]
async fn test_new_entry_prefers_terminal(cx: &mut TestAppContext) {
    let (_fs, project) = init_multi_project_test(&["/project-a"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let workspace = multi_workspace.read_with(cx, |multi_workspace, _cx| {
        multi_workspace.workspace().clone()
    });

    sidebar.read_with(cx, |sidebar, cx| {
        assert!(
            sidebar.should_create_terminal_for_workspace(&workspace, cx),
            "the sidebar's new entry must open a terminal"
        );
    });
}

#[gpui::test]
async fn test_worktree_rows_carry_a_group_key_for_closed_workspaces(cx: &mut TestAppContext) {
    let (_fs, project) = init_multi_project_test(&["/project-a"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);

    let workspaces: Vec<_> =
        multi_workspace.read_with(cx, |mw, _cx| mw.workspaces().cloned().collect());
    let tree = cx
        .update(|_window, cx| workspace_manager::build_tree(&workspaces, &HashMap::new(), &[], cx));

    let worktrees: Vec<_> = tree
        .groups
        .iter()
        .flat_map(|group| group.projects.iter())
        .flat_map(|project| project.worktrees.iter())
        .collect();

    assert!(
        !worktrees.is_empty(),
        "this assertion keeps the loop below from passing vacuously"
    );

    for worktree in &worktrees {
        assert!(
            worktree.group_key.is_some(),
            "worktree {:?} has no group key, so it would be unreachable once its \
             workspace closes and its WeakEntity stops upgrading",
            worktree.name
        );
        assert_ne!(
            worktree.name.as_ref(),
            "(detached)",
            "a worktree must never be labelled by the absent branch of a detached HEAD"
        );
    }
}

/// A restored window reopens only its active workspace; every other project
/// group comes back as a key with no workspace behind it. Those groups must
/// keep their rows — a bar that only lists open workspaces loses a project on
/// every restart, which reads as data loss even though the group survived.
#[gpui::test]
async fn test_a_closed_project_group_keeps_its_row(cx: &mut TestAppContext) {
    let (_fs, project) = init_multi_project_test(&["/project-a"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);

    let workspaces: Vec<_> =
        multi_workspace.read_with(cx, |mw, _cx| mw.workspaces().cloned().collect());
    let closed = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/gone-project")]));
    // The open workspace's own group arrives in the closed list too — restore
    // hands back every group it kept — and must not double its row.
    let open_duplicate =
        workspaces[0].read_with(cx, |workspace, cx| workspace.project_group_key(cx));
    let tree = cx.update(|_window, cx| {
        workspace_manager::build_tree(
            &workspaces,
            &HashMap::new(),
            &[closed.clone(), open_duplicate],
            cx,
        )
    });

    let projects: Vec<_> = tree
        .groups
        .iter()
        .flat_map(|group| group.projects.iter())
        .collect();
    let gone = projects
        .iter()
        .find(|project| project.name.as_ref() == "gone-project")
        .expect("the closed group's project must keep its row");
    assert_eq!(gone.worktrees.len(), 1);
    assert!(
        gone.worktrees[0].workspace.is_none(),
        "nothing is open for it, so the row must carry no workspace"
    );
    assert_eq!(
        gone.worktrees[0].group_key.as_ref(),
        Some(&closed),
        "the key is how clicking the row opens the group"
    );
    let project_a_rows = projects
        .iter()
        .flat_map(|project| project.worktrees.iter())
        .filter(|worktree| {
            worktree.folder_root.as_deref() == Some(std::path::Path::new("/project-a"))
        })
        .count();
    assert_eq!(
        project_a_rows, 1,
        "an open workspace whose group also arrives as closed keeps one row, not two"
    );
}

#[gpui::test]
async fn test_closed_main_group_does_not_duplicate_open_linked_worktree(cx: &mut TestAppContext) {
    agent_workspaces::test_support::init_test(cx);
    let fs = FakeFs::new(cx.executor());
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = project::Project::test(fs, [], cx).await;
    project.update(cx, |project, cx| {
        project.add_test_remote_worktree_with_repository(
            "/worktrees/disable-whop-payments",
            Some("/projects/viral-studio/.git"),
            cx,
        );
    });

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspaces = multi_workspace.read_with(cx, |multi_workspace, _| {
        multi_workspace.workspaces().cloned().collect::<Vec<_>>()
    });
    let main_key = ProjectGroupKey::new(
        None,
        PathList::new(&[PathBuf::from("/projects/viral-studio")]),
    );
    let tree = cx.update(|_, cx| {
        workspace_manager::build_tree(&workspaces, &HashMap::new(), &[main_key], cx)
    });
    let projects = tree
        .groups
        .iter()
        .flat_map(|group| &group.projects)
        .collect::<Vec<_>>();

    assert_eq!(
        projects.len(),
        1,
        "the restored main key and its open linked checkout are one project: {:?}",
        projects
            .iter()
            .map(|project| project.name.as_ref())
            .collect::<Vec<_>>()
    );
    assert_eq!(projects[0].name.as_ref(), "viral-studio");
    assert_eq!(projects[0].worktrees.len(), 1);
    assert_eq!(
        projects[0].worktrees[0].folder_root.as_deref(),
        Some(Path::new("/worktrees/disable-whop-payments"))
    );
}

#[gpui::test]
async fn test_restored_linked_workspace_stays_under_main_before_git_discovery(
    cx: &mut TestAppContext,
) {
    agent_workspaces::test_support::init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/worktrees/disable-whop-payments",
        serde_json::json!({ "src": {} }),
    )
    .await;
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project =
        project::Project::test(fs, ["/worktrees/disable-whop-payments".as_ref()], cx).await;
    assert!(project.read_with(cx, |project, cx| project.repositories(cx).is_empty()));
    assert!(project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .next()
            .is_some_and(|worktree| worktree.read(cx).root_repo_common_dir().is_none())
    }));

    let main_key = ProjectGroupKey::new(
        None,
        PathList::new(&[PathBuf::from("/projects/viral-studio")]),
    );
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    workspace.update(cx, |workspace, _| {
        workspace.test_set_project_group_key_hint(main_key.clone());
    });
    let workspaces = vec![workspace];
    let tree = cx.update(|_, cx| {
        workspace_manager::build_tree(
            &workspaces,
            &HashMap::new(),
            std::slice::from_ref(&main_key),
            cx,
        )
    });
    let projects = tree
        .groups
        .iter()
        .flat_map(|group| &group.projects)
        .collect::<Vec<_>>();

    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].name.as_ref(), "viral-studio");
    assert_eq!(
        projects[0].worktrees[0].folder_root.as_deref(),
        Some(Path::new("/worktrees/disable-whop-payments"))
    );
    assert_eq!(
        projects[0].worktrees[0].name.as_ref(),
        "disable-whop-payments"
    );
}

#[gpui::test]
fn test_cached_linked_worktree_restores_under_its_repository(cx: &mut TestAppContext) {
    let main_root = PathBuf::from("/projects/viral-studio");
    let common_dir = main_root.join(".git");
    let linked_root = PathBuf::from("/worktrees/disable-whop-payments");
    let available_worktrees = HashMap::from([(
        (common_dir.clone(), None),
        vec![
            git::repository::Worktree {
                path: main_root.clone(),
                ref_name: Some("refs/heads/main".into()),
                sha: "main-sha".into(),
                is_main: true,
                is_bare: false,
            },
            git::repository::Worktree {
                path: linked_root.clone(),
                ref_name: Some("refs/heads/disable-whop-payments".into()),
                sha: "linked-sha".into(),
                is_main: false,
                is_bare: false,
            },
        ],
    )]);
    let stale_linked_key =
        ProjectGroupKey::new(None, PathList::new(std::slice::from_ref(&linked_root)));
    let tree = cx.update(|cx| {
        workspace_manager::build_tree(
            &[],
            &available_worktrees,
            std::slice::from_ref(&stale_linked_key),
            cx,
        )
    });
    let projects = tree
        .groups
        .iter()
        .flat_map(|group| &group.projects)
        .collect::<Vec<_>>();

    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].name.as_ref(), "viral-studio");
    assert_eq!(projects[0].key.as_ref(), common_dir.as_path());
    assert_eq!(projects[0].worktrees.len(), 1);
    assert_eq!(
        projects[0].worktrees[0].name.as_ref(),
        "disable-whop-payments"
    );
    assert_eq!(
        projects[0].worktrees[0]
            .group_key
            .as_ref()
            .expect("restored worktree should retain its project key")
            .path_list(),
        &PathList::new(std::slice::from_ref(&main_root)),
    );
}

#[gpui::test]
async fn test_remove_project_removes_every_persisted_worktree_group(cx: &mut TestAppContext) {
    let (fs, main_project) = init_multi_project_test(
        &["/projects/viral-studio", "/worktrees/disable-whop-payments"],
        cx,
    )
    .await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(main_project, window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);
    add_test_project(
        "/worktrees/disable-whop-payments",
        &fs,
        &multi_workspace,
        cx,
    )
    .await;

    let main_root = PathBuf::from("/projects/viral-studio");
    let linked_root = PathBuf::from("/worktrees/disable-whop-payments");
    let available_worktrees = HashMap::from([(
        (main_root.join(".git"), None),
        vec![
            git::repository::Worktree {
                path: main_root.clone(),
                ref_name: Some("refs/heads/main".into()),
                sha: "main-sha".into(),
                is_main: true,
                is_bare: false,
            },
            git::repository::Worktree {
                path: linked_root,
                ref_name: Some("refs/heads/disable-whop-payments".into()),
                sha: "linked-sha".into(),
                is_main: false,
                is_bare: false,
            },
        ],
    )]);
    let group_keys = multi_workspace.read_with(cx, |multi_workspace, _| {
        multi_workspace.project_group_keys()
    });
    let tree = cx
        .update(|_, cx| workspace_manager::build_tree(&[], &available_worktrees, &group_keys, cx));
    let project = tree
        .groups
        .iter()
        .flat_map(|group| &group.projects)
        .find(|project| project.name.as_ref() == "viral-studio")
        .expect("the persisted worktree groups should render as one project");
    let removal_keys = tree.project_group_keys(project.id);
    assert!(group_keys.iter().all(|group_key| {
        removal_keys
            .iter()
            .any(|removal_key| removal_key.matches(group_key))
    }));

    multi_workspace
        .update_in(cx, |multi_workspace, window, cx| {
            multi_workspace.remove_project_groups(removal_keys, window, cx)
        })
        .await
        .expect("project removal should succeed");
    cx.run_until_parked();

    multi_workspace.read_with(cx, |multi_workspace, cx| {
        assert!(
            multi_workspace
                .workspaces()
                .all(|workspace| workspace.read(cx).root_paths(cx).is_empty()),
            "removing the project must not leave disable-whop-payments behind"
        );
    });
}

#[gpui::test]
async fn test_same_path_on_different_remote_hosts_stays_separate(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    init_test(cx);
    cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        workspace::init(app_state.clone(), cx);
        app_state
    });
    let server_fs = FakeFs::new(server_cx.executor());
    server_fs
        .insert_tree("/project", serde_json::json!({ ".git": {}, "src": {} }))
        .await;

    let (project_a, _headless_a, connection_a) = start_remote_project(
        &server_fs,
        Path::new("/project"),
        &app_state,
        None,
        cx,
        server_cx,
    )
    .await;
    let (project_b, _headless_b, connection_b) = start_remote_project(
        &server_fs,
        Path::new("/project"),
        &app_state,
        None,
        cx,
        server_cx,
    )
    .await;
    for project in [&project_a, &project_b] {
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
    }

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project_a, window, cx));
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(project_b, window, cx);
    });
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let available_worktrees =
        sidebar.read_with(cx, |sidebar, _| sidebar.available_worktrees.clone());
    let expected_cache_keys = [
        workspace_manager::repository_cache_key(Path::new("/project/.git"), Some(&connection_a)),
        workspace_manager::repository_cache_key(Path::new("/project/.git"), Some(&connection_b)),
    ];
    assert!(
        expected_cache_keys
            .iter()
            .all(|key| available_worktrees.contains_key(key)),
        "repository discovery must not overwrite another host's same-path cache entry: {:?}",
        available_worktrees.keys().collect::<Vec<_>>()
    );
    let workspaces = multi_workspace.read_with(cx, |multi_workspace, _| {
        multi_workspace.workspaces().cloned().collect::<Vec<_>>()
    });
    let tree = cx
        .update(|_, cx| workspace_manager::build_tree(&workspaces, &available_worktrees, &[], cx));
    let projects = tree
        .groups
        .iter()
        .flat_map(|group| &group.projects)
        .collect::<Vec<_>>();

    assert_eq!(
        projects.len(),
        2,
        "identical paths on different hosts are different projects"
    );
    assert!(
        projects.iter().all(|project| project.worktrees.len() == 1),
        "a host's checkout must not appear under another host's project"
    );

    let key_a = ProjectGroupKey::new(
        Some(connection_a),
        PathList::new(&[PathBuf::from("/project")]),
    );
    let key_b = ProjectGroupKey::new(
        Some(connection_b),
        PathList::new(&[PathBuf::from("/project")]),
    );
    multi_workspace.read_with(cx, |multi_workspace, cx| {
        let workspace_a =
            workspace_for_scoped_root(multi_workspace, Path::new("/project"), Some(&key_a), cx)
                .expect("host A row should find host A workspace");
        let workspace_b =
            workspace_for_scoped_root(multi_workspace, Path::new("/project"), Some(&key_b), cx)
                .expect("host B row should find host B workspace");
        assert_eq!(
            workspace_a.read(cx).project_group_key(cx).host(),
            key_a.host()
        );
        assert_eq!(
            workspace_b.read(cx).project_group_key(cx).host(),
            key_b.host()
        );
        assert_ne!(workspace_a.entity_id(), workspace_b.entity_id());
    });
}

#[gpui::test]
async fn test_scoped_workspace_lookup_checks_every_root(cx: &mut TestAppContext) {
    agent_workspaces::test_support::init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/first", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/second", serde_json::json!({ "src": {} }))
        .await;
    let project = project::Project::test(fs, [Path::new("/first"), Path::new("/second")], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    multi_workspace.read_with(cx, |multi_workspace, cx| {
        let key = multi_workspace.workspace().read(cx).project_group_key(cx);
        let found =
            workspace_for_scoped_root(multi_workspace, Path::new("/second"), Some(&key), cx)
                .expect("a non-first root should resolve its workspace");
        assert_eq!(found.entity_id(), multi_workspace.workspace().entity_id());
    });
}

#[gpui::test]
async fn test_multi_root_workspace_builds_every_root(cx: &mut TestAppContext) {
    agent_workspaces::test_support::init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/first", serde_json::json!({ "src": {} }))
        .await;
    fs.insert_tree("/second", serde_json::json!({ "src": {} }))
        .await;
    let project = project::Project::test(fs, [Path::new("/first"), Path::new("/second")], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspaces = multi_workspace.read_with(cx, |multi_workspace, _| {
        multi_workspace.workspaces().cloned().collect::<Vec<_>>()
    });
    let workspace_id = workspaces[0].entity_id();
    let tree =
        cx.update(|_, cx| workspace_manager::build_tree(&workspaces, &HashMap::new(), &[], cx));
    let roots = tree
        .groups
        .iter()
        .flat_map(|group| &group.projects)
        .flat_map(|project| &project.worktrees)
        .filter_map(|worktree| worktree.folder_root.clone())
        .collect::<Vec<_>>();

    assert_eq!(
        roots,
        vec![PathBuf::from("/first"), PathBuf::from("/second")],
        "every root in a multi-root workspace must remain reachable in the sidebar",
    );
    assert!(
        tree.groups
            .iter()
            .flat_map(|group| &group.projects)
            .flat_map(|project| &project.worktrees)
            .all(|worktree| worktree
                .workspace
                .as_ref()
                .and_then(|workspace| workspace.upgrade())
                .is_some_and(|workspace| workspace.entity_id() == workspace_id)),
        "each root row must activate the shared multi-root workspace",
    );
}

/// The tree is keyed by repository, so a project under no version control has
/// no key to contribute — and used to disappear from the sidebar entirely.
#[gpui::test]
async fn test_project_without_a_repository_still_appears(cx: &mut TestAppContext) {
    let (fs, project) = init_multi_project_test(&["/project-a"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);

    fs.insert_tree("/plain-folder", serde_json::json!({ "src": {} }))
        .await;
    add_test_project("/plain-folder", &fs, &multi_workspace, cx).await;

    let workspaces: Vec<_> =
        multi_workspace.read_with(cx, |mw, _cx| mw.workspaces().cloned().collect());
    let tree = cx
        .update(|_window, cx| workspace_manager::build_tree(&workspaces, &HashMap::new(), &[], cx));
    let names: Vec<_> = tree
        .groups
        .iter()
        .flat_map(|group| group.projects.iter())
        .map(|project| project.name.to_string())
        .collect();

    assert!(
        names.contains(&"plain-folder".to_string()),
        "a project with no Git repository vanished from the tree; got {names:?}"
    );
}

#[gpui::test]
async fn test_fallback_worktrees_use_main_label(cx: &mut TestAppContext) {
    agent_workspaces::test_support::init_test(cx);
    let fs = FakeFs::new(cx.executor());
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    let project = project::Project::test(fs.clone(), [], cx).await;
    project.update(cx, |project, cx| {
        project.add_test_remote_worktree_with_repository(
            "/git-project",
            Some("/git-project/.git"),
            cx,
        );
    });
    assert!(project.read_with(cx, |project, cx| project.repositories(cx).is_empty()));
    assert!(project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .next()
            .is_some_and(|worktree| worktree.read(cx).root_repo_common_dir().is_some())
    }));

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);

    fs.insert_tree("/non-git-project", serde_json::json!({ "src": {} }))
        .await;
    add_test_project("/non-git-project", &fs, &multi_workspace, cx).await;

    let workspaces: Vec<_> = multi_workspace.read_with(cx, |multi_workspace, _| {
        multi_workspace.workspaces().cloned().collect()
    });
    let closed = ProjectGroupKey::new(None, PathList::new(&[PathBuf::from("/closed-project")]));
    let tree = cx.update(|_window, cx| {
        workspace_manager::build_tree(&workspaces, &HashMap::new(), &[closed], cx)
    });
    let rows: Vec<_> = tree
        .rows()
        .into_iter()
        .map(|row| (row.depth, row.label.to_string()))
        .collect();

    assert_eq!(
        rows,
        vec![
            (0, "closed-project".to_owned()),
            (1, "main".to_owned()),
            (0, "git-project".to_owned()),
            (1, "main".to_owned()),
            (0, "non-git-project".to_owned()),
            (1, "main".to_owned()),
        ]
    );
}

/// Offering New Worktree on a project with no repository did nothing at all —
/// `create_worktree` bails with "no git repository in the project".
#[gpui::test]
async fn test_a_project_without_a_repository_cannot_create_worktrees(cx: &mut TestAppContext) {
    let (fs, project) = init_multi_project_test(&["/project-a"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);

    fs.insert_tree("/plain-folder", serde_json::json!({ "src": {} }))
        .await;
    add_test_project("/plain-folder", &fs, &multi_workspace, cx).await;

    let workspaces: Vec<_> =
        multi_workspace.read_with(cx, |mw, _cx| mw.workspaces().cloned().collect());
    let tree = cx
        .update(|_window, cx| workspace_manager::build_tree(&workspaces, &HashMap::new(), &[], cx));

    let by_name: Vec<_> = tree
        .groups
        .iter()
        .flat_map(|group| group.projects.iter())
        .map(|project| (project.name.to_string(), project.has_repository))
        .collect();

    assert_eq!(
        by_name,
        vec![
            ("plain-folder".to_owned(), false),
            ("project-a".to_owned(), true),
        ]
    );
}

/// Orca's star is not a favourite: it marks the repository's original clone
/// directory, tooltip "Primary worktree (original clone directory)".
#[gpui::test]
async fn test_the_star_marks_the_original_clone(cx: &mut TestAppContext) {
    let (_fs, project) = init_multi_project_test(&["/project-a"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);

    let workspaces: Vec<_> =
        multi_workspace.read_with(cx, |mw, _cx| mw.workspaces().cloned().collect());
    let tree = cx
        .update(|_window, cx| workspace_manager::build_tree(&workspaces, &HashMap::new(), &[], cx));

    let starred: Vec<_> = tree
        .groups
        .iter()
        .flat_map(|group| group.projects.iter())
        .flat_map(|project| project.worktrees.iter())
        .map(|worktree| worktree.is_primary)
        .collect();

    assert_eq!(
        starred,
        vec![true],
        "the checkout holding .git is the primary worktree and must carry the star"
    );
}

#[gpui::test]
async fn test_main_worktree_opened_through_a_symlink_is_not_removable(cx: &mut TestAppContext) {
    agent_workspaces::test_support::init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/real-project",
        serde_json::json!({ ".git": {}, "src": {} }),
    )
    .await;
    fs.insert_symlink("/project-alias", PathBuf::from("/real-project"))
        .await;
    let project = project::Project::test(fs, [Path::new("/project-alias")], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspaces: Vec<_> = multi_workspace.read_with(cx, |multi_workspace, _| {
        multi_workspace.workspaces().cloned().collect()
    });
    let available_worktrees = HashMap::from([(
        (PathBuf::from("/real-project/.git"), None),
        vec![git::repository::Worktree {
            path: PathBuf::from("/real-project"),
            ref_name: Some("refs/heads/main".into()),
            sha: "abc".into(),
            is_main: true,
            is_bare: false,
        }],
    )]);
    let tree = cx
        .update(|_, cx| workspace_manager::build_tree(&workspaces, &available_worktrees, &[], cx));
    assert_eq!(tree.groups.len(), 1);
    assert_eq!(tree.groups[0].projects.len(), 1);
    assert_eq!(tree.groups[0].projects[0].worktrees.len(), 1);
    let worktree = tree.groups[0].projects[0].worktrees[0].id;

    assert!(tree.worktree_is_primary(worktree));
    assert_eq!(tree.removable_worktree_root(worktree), None);
}

#[gpui::test]
async fn test_duplicate_workspaces_share_one_checkout_row(cx: &mut TestAppContext) {
    let (fs, project) = init_multi_project_test(&["/project-a"], cx).await;
    let duplicate =
        project::Project::test(fs as Arc<dyn fs::Fs>, ["/project-a".as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);
    multi_workspace.update_in(cx, |multi_workspace, window, cx| {
        multi_workspace.test_add_workspace(duplicate, window, cx);
    });
    cx.run_until_parked();

    let workspaces = multi_workspace.read_with(cx, |multi_workspace, _| {
        multi_workspace.workspaces().cloned().collect::<Vec<_>>()
    });
    let tree =
        cx.update(|_, cx| workspace_manager::build_tree(&workspaces, &HashMap::new(), &[], cx));
    let rows = tree
        .groups
        .iter()
        .flat_map(|group| &group.projects)
        .flat_map(|project| &project.worktrees)
        .filter(|worktree| worktree.folder_root.as_deref() == Some(Path::new("/project-a")))
        .count();

    assert_eq!(rows, 1, "one checkout must have one sidebar row");
}

/// A pin has to outlive the session, like the groups and the collapse state.
#[gpui::test]
async fn test_pins_survive_a_restart(cx: &mut TestAppContext) {
    let (_fs, project) = init_multi_project_test(&["/project-a"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let state = sidebar.update(cx, |sidebar, cx| {
        sidebar.toggle_worktree_pinned(PathBuf::from("/project-a"), None, cx);
        sidebar.serialized_state(cx)
    });
    let state = state.expect("the sidebar must serialize its state");

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.pinned_worktrees.clear();
        sidebar.restore_serialized_state(&state, window, cx);
    });

    sidebar.read_with(cx, |sidebar, _cx| {
        assert_eq!(
            sidebar.pinned_worktrees,
            vec![workspace_manager::ScopedPath::new(
                PathBuf::from("/project-a"),
                None,
            )],
            "restoring the serialized state did not bring the pin back"
        );
    });

    // Unpinning is the same toggle, and must also persist.
    let state = sidebar.update(cx, |sidebar, cx| {
        sidebar.toggle_worktree_pinned(PathBuf::from("/project-a"), None, cx);
        sidebar.serialized_state(cx)
    });
    let state = state.expect("the sidebar must serialize its state");
    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.restore_serialized_state(&state, window, cx);
    });
    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(sidebar.pinned_worktrees.is_empty());
    });
}

#[gpui::test]
async fn test_legacy_sidebar_paths_migrate_only_when_host_is_unambiguous(cx: &mut TestAppContext) {
    let (_fs, project) = init_multi_project_test(&["/project-a"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);
    let legacy = r#"{
        "pinned_worktrees":["/project-a"],
        "unread_worktrees":["/project-a"],
        "hidden_worktrees":["/project-a"],
        "workspace_groups":[{"name":"legacy","projects":["/project-a"]}]
    }"#;

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.restore_serialized_state(legacy, window, cx);
        sidebar.workspace_tree(cx);
    });
    let migrated = sidebar
        .read_with(cx, |sidebar, cx| sidebar.serialized_state(cx))
        .expect("sidebar state should serialize");
    let migrated: serde_json::Value =
        serde_json::from_str(&migrated).expect("migrated sidebar state should be JSON");
    for field in ["pinned_worktrees", "unread_worktrees", "hidden_worktrees"] {
        assert_eq!(migrated[field][0]["path"], "/project-a");
        assert!(migrated[field][0]["host_key"].is_null());
    }
    assert_eq!(
        migrated["workspace_groups"][0]["projects"][0]["path"],
        "/project-a"
    );

    sidebar.update(cx, |_sidebar, _| {
        let ambiguous = HashSet::from([
            (PathBuf::from("/same"), Some("mock:1".to_owned())),
            (PathBuf::from("/same"), Some("mock:2".to_owned())),
        ]);
        assert_eq!(
            workspace_manager::ScopedPath::Legacy(PathBuf::from("/same")).resolved(&ambiguous),
            None,
            "legacy path-only state must not pick one of two hosts"
        );
    });
}

/// A linked worktree must join the project it belongs to. `ProjectGroupKey`
/// carries the MAIN worktree's path, not the checkout's, so matching it against
/// the registered repository's work directory never succeeded and every linked
/// worktree fell into the no-version-control fallback as a project of its own.
///
/// The checkout directory is deliberately named differently from the branch so
/// the sidebar cannot accidentally regress to displaying the branch name.
#[gpui::test]
async fn test_a_linked_worktree_joins_its_repository(cx: &mut TestAppContext) {
    let (project, fs) = init_test_project_with_git("/project", cx).await;

    fs.as_fake()
        .add_linked_worktree_for_repo(
            Path::new("/project/.git"),
            false,
            git::repository::Worktree {
                path: std::path::PathBuf::from("/wt/checkout-1"),
                ref_name: Some("refs/heads/rosewood".into()),
                sha: "abc".into(),
                is_main: false,
                is_bare: false,
            },
        )
        .await;
    fs.as_fake()
        .set_branch_name(Path::new("/project/.git"), Some("master"));
    fs.as_fake()
        .set_branch_name(Path::new("/wt/checkout-1/.git"), Some("rosewood"));
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);

    let worktree_workspace = add_test_project(
        "/wt/checkout-1",
        &fs.as_fake().clone(),
        &multi_workspace,
        cx,
    )
    .await;
    let scans = worktree_workspace.update(cx, |workspace, cx| {
        workspace
            .project()
            .update(cx, |project, cx| project.git_scans_complete(cx))
    });
    scans.await;
    cx.run_until_parked();

    let workspaces: Vec<_> =
        multi_workspace.read_with(cx, |mw, _cx| mw.workspaces().cloned().collect());
    let tree = cx
        .update(|_window, cx| workspace_manager::build_tree(&workspaces, &HashMap::new(), &[], cx));
    let tree_shape = |tree: &workspace_manager::WorkspaceTree| {
        tree.groups
            .iter()
            .flat_map(|group| &group.projects)
            .map(|project| {
                (
                    project.key.clone(),
                    project.name.clone(),
                    project
                        .worktrees
                        .iter()
                        .map(|worktree| (worktree.name.clone(), worktree.folder_root.clone()))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
    };
    let mut reversed_workspaces = workspaces;
    reversed_workspaces.reverse();
    let reversed_tree = cx.update(|_, cx| {
        workspace_manager::build_tree(&reversed_workspaces, &HashMap::new(), &[], cx)
    });
    assert_eq!(
        tree_shape(&tree),
        tree_shape(&reversed_tree),
        "discovery order must not change project grouping or worktree order"
    );

    let projects: Vec<_> = tree
        .groups
        .iter()
        .flat_map(|group| group.projects.iter())
        .collect();
    assert_eq!(
        projects.len(),
        1,
        "the linked worktree formed a second project instead of joining its repository: {:?}",
        projects
            .iter()
            .map(|p| p.name.to_string())
            .collect::<Vec<_>>()
    );
    assert!(projects[0].has_repository);

    let labels: Vec<_> = projects[0]
        .worktrees
        .iter()
        .map(|worktree| worktree.name.to_string())
        .collect();
    assert!(
        labels.contains(&"checkout-1".to_owned()),
        "the linked worktree must be labelled by its directory, not its branch: {labels:?}"
    );

    let primary: Vec<_> = projects[0]
        .worktrees
        .iter()
        .map(|worktree| worktree.is_primary)
        .collect();
    assert_eq!(
        primary.iter().filter(|is_primary| **is_primary).count(),
        1,
        "exactly one worktree is the original clone; got {primary:?} for {labels:?}"
    );
}

/// A repository is registered asynchronously, after the worktree scan. Until it
/// arrives the workspace has no repository and build_tree can only place it by
/// its own root, so the tree has to rebuild once the repository shows up.
#[gpui::test]
async fn test_the_tree_rebuilds_when_a_repository_appears(cx: &mut TestAppContext) {
    let (_fs, project) = init_multi_project_test(&["/project-a"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let git_store = multi_workspace.read_with(cx, |mw, cx| {
        mw.workspace()
            .read(cx)
            .project()
            .read(cx)
            .git_store()
            .clone()
    });

    let rebuilt = Rc::new(std::cell::Cell::new(false));
    let _subscription = cx.update(|_window, cx| {
        let rebuilt = rebuilt.clone();
        cx.observe(&sidebar, move |_, _| rebuilt.set(true))
    });

    git_store.update(cx, |_, cx| {
        cx.emit(project::git_store::GitStoreEvent::RepositoryAdded);
    });
    cx.run_until_parked();

    assert!(
        rebuilt.get(),
        "a repository appearing after the tree was built must rebuild it, or the \
         worktree stays stuck in the no-version-control fallback as its own project"
    );
}

/// Orca clears the unread dot when the worktree is activated, not only from
/// the menu.
#[gpui::test]
async fn test_activating_a_worktree_clears_its_unread_dot(cx: &mut TestAppContext) {
    let (_fs, project) = init_multi_project_test(&["/project-a"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    sidebar.update(cx, |sidebar, cx| {
        sidebar.toggle_worktree_unread(PathBuf::from("/project-a"), None, cx);
        assert_eq!(
            sidebar.unread_worktrees,
            vec![workspace_manager::ScopedPath::new(
                PathBuf::from("/project-a"),
                None,
            )]
        );
        sidebar.clear_worktree_unread(Path::new("/project-a"), None, cx);
    });

    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            sidebar.unread_worktrees.is_empty(),
            "activating the worktree must clear its dot"
        );
    });
}

/// A folder of checkouts (Orca's own layout) used to scatter a worktree row of
/// itself under every repository beneath it.
#[gpui::test]
async fn test_a_folder_of_repositories_is_one_project(cx: &mut TestAppContext) {
    let (fs, project) = init_multi_project_test(&["/project-a"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let _sidebar = setup_sidebar(&multi_workspace, cx);

    fs.insert_tree(
        "/container",
        serde_json::json!({
            "first-checkout": { ".git": {}, "src": {} },
            "second-checkout": { ".git": {}, "src": {} },
        }),
    )
    .await;
    add_test_project("/container", &fs, &multi_workspace, cx).await;

    let workspaces: Vec<_> =
        multi_workspace.read_with(cx, |mw, _cx| mw.workspaces().cloned().collect());
    let tree = cx
        .update(|_window, cx| workspace_manager::build_tree(&workspaces, &HashMap::new(), &[], cx));
    let names: Vec<_> = tree
        .groups
        .iter()
        .flat_map(|group| group.projects.iter())
        .map(|project| project.name.to_string())
        .collect();

    assert_eq!(
        names,
        vec!["container".to_owned(), "project-a".to_owned()],
        "the repositories inside a project folder are its contents, not projects"
    );
}

/// Collapse is part of the arrangement the user built; it used to live only in
/// memory and reset on every restart.
#[gpui::test]
async fn test_collapsed_nodes_survive_a_restart(cx: &mut TestAppContext) {
    let (_fs, project) = init_multi_project_test(&["/project-a"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    let state = sidebar.update(cx, |sidebar, cx| {
        let tree = sidebar.workspace_tree(cx);
        let project = tree.groups[0].projects[0].id;
        let key =
            Sidebar::collapse_key_for_row(&tree, &workspace_manager::RowKind::Project(project))
                .expect("project row should collapse");
        sidebar.toggle_workspace_node_collapsed(key, cx);
        sidebar.serialized_state(cx)
    });
    let state = state.expect("the sidebar must serialize its state");
    let serialized: SerializedSidebar =
        serde_json::from_str(&state).expect("the serialized sidebar state should decode");
    assert!(
        serialized
            .collapsed_projects
            .iter()
            .any(|path| path.matches(Path::new("/project-a/.git"), None)),
        "the collapsed node was not written to the serialized state: {state}"
    );

    sidebar.update_in(cx, |sidebar, window, cx| {
        sidebar.collapsed_projects.clear();
        sidebar.restore_serialized_state(&state, window, cx);
    });

    sidebar.read_with(cx, |sidebar, _cx| {
        assert!(
            !sidebar.collapsed_projects.is_empty(),
            "restoring the serialized state did not bring the collapsed node back"
        );
    });
}

#[gpui::test]
async fn test_legacy_project_collapse_migrates_to_host_scoped_state(cx: &mut TestAppContext) {
    let (_fs, project) = init_multi_project_test(&["/project-a"], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let sidebar = setup_sidebar(&multi_workspace, cx);

    sidebar.update(cx, |sidebar, cx| {
        sidebar
            .collapsed_workspace_nodes
            .insert(SharedString::from("/project-a"));
        let tree = sidebar.workspace_tree(cx);
        assert!(tree.groups[0].projects[0].collapsed);
        assert!(
            !sidebar
                .collapsed_workspace_nodes
                .contains(&SharedString::from("/project-a"))
        );
        assert_eq!(sidebar.collapsed_projects.len(), 1);
    });
}
