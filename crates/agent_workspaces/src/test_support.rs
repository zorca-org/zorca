use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    Pixels, Render, TestAppContext, VisualTestContext, Window, div, px,
};
use settings::SettingsStore;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use workspace::{MultiWorkspace, Sidebar as WorkspaceSidebar, SidebarEvent, SidebarSide};

pub fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        // Use an isolated DB so parallel tests can't see each other's
        // persisted records (e.g. created-worktree records).
        cx.set_global(db::AppDatabase::test_new());
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        release_channel::init("0.0.0".parse().unwrap(), cx);
        crate::worktree_metadata_store::WorktreeMetadataStore::init_global(cx);
        crate::terminal_thread_metadata_store::TerminalThreadMetadataStore::init_global(cx);
    });
}

/// Returns the creation time assigned to a linked worktree's git metadata
/// directory, mirroring `FakeGitRepository::worktree_created_at` (which uses
/// the FakeFs directory mtime as a stand-in for the creation time).
pub async fn fake_worktree_created_at(fs: &dyn fs::Fs, worktree_path: &Path) -> SystemTime {
    let git_file = fs.load(&worktree_path.join(".git")).await.unwrap();
    let git_dir = worktree_path.join(git_file.strip_prefix("gitdir:").unwrap().trim());
    let (seconds, nanos) = fs
        .metadata(&git_dir)
        .await
        .unwrap()
        .unwrap()
        .mtime
        .to_seconds_and_nanos_for_persistence()
        .unwrap();
    UNIX_EPOCH + Duration::new(seconds, nanos)
}

/// Records the worktree in the created-worktrees registry with its actual
/// (fake) creation time, as the worktree creation flow would. Tests that
/// expect a worktree to be archivable must call this after setting it up.
pub async fn record_zed_created_worktree(
    fs: &dyn fs::Fs,
    worktree_path: &Path,
    remote: Option<&remote::RemoteConnectionOptions>,
    cx: &mut TestAppContext,
) {
    let created_at = fake_worktree_created_at(fs, worktree_path).await;
    cx.update(|cx| {
        git_ui::created_worktrees::record_created_worktree(worktree_path, remote, created_at, cx)
    })
    .await
    .unwrap();
}

pub struct TestWorkspaceSidebar {
    focus_handle: FocusHandle,
    threads_list_active: bool,
}

impl TestWorkspaceSidebar {
    fn new(threads_list_active: bool, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            threads_list_active,
        }
    }
}

impl EventEmitter<SidebarEvent> for TestWorkspaceSidebar {}

impl Focusable for TestWorkspaceSidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl WorkspaceSidebar for TestWorkspaceSidebar {
    fn width(&self, _cx: &App) -> Pixels {
        px(300.)
    }

    fn set_width(&mut self, _width: Option<Pixels>, _cx: &mut Context<Self>) {}

    fn has_notifications(&self, _cx: &App) -> bool {
        false
    }

    fn side(&self, _cx: &App) -> SidebarSide {
        SidebarSide::Left
    }

    fn is_threads_list_view_active(&self) -> bool {
        self.threads_list_active
    }
}

impl Render for TestWorkspaceSidebar {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

pub fn register_test_sidebar(
    threads_list_active: bool,
    cx: &mut VisualTestContext,
) -> Entity<TestWorkspaceSidebar> {
    cx.update(|window, cx| {
        let multi_workspace = window
            .root::<MultiWorkspace>()
            .flatten()
            .expect("test window should have a MultiWorkspace root");
        let sidebar = cx.new(|cx| TestWorkspaceSidebar::new(threads_list_active, cx));
        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.register_sidebar(sidebar.clone(), cx);
        });
        sidebar
    })
}
