//! The workspace sidebar: an on-demand tab, opened from the command palette by
//! `ade_workspaces::ToggleWorkspacesView`, that makes the *workspace*, not the
//! file, the primary navigation object. It is a dev view — never resident, and
//! deliberately not serializable, so it does not come back after a restart.
//!
//! The tree is two levels deep — project groups over workspace rows — and the
//! dot on each row is the one honest thing on screen: it is whatever the last
//! [`WorkspaceLifecycleService::reconcile_all`] pass found in the session
//! backend, not what the registry remembered. That pass, and the status stream
//! that drives it, belong to [`AdeWorkspaceStore`] — this panel observes the
//! store and asks it to refresh after an action, so a second view of the same
//! data (the ledger sidebar) cannot displace this one's status stream.
//!
//! Three rules from the lifecycle layer show through in the UI and must keep
//! showing through:
//!
//! - **Selecting opens; a row with no session gets one.** A workspace with no
//!   session is a normal row, not a "gone" state, and its first terminal is an
//!   ordinary attach-or-create into the record the row already names.
//! - **"Delete Workspace" is the one destructive entry, and it is
//!   unconfirmed.** It kills every session in the workspace (a single daemon
//!   `KillWorkspace`) and forgets the row — there is no separate "Remove from
//!   list" any more, because a registry-only removal would leave the daemon
//!   record and its sessions running behind a row nobody can see. "Stop
//!   (detach)" sits next to it and does not kill.
//! - **Every lifecycle call blocks.** The session backend is synchronous and
//!   the registry is sqlite, so nothing in this file may call the service on
//!   the foreground thread; it all goes through `cx.background_spawn`.
//!
//! **Remote workspaces are ordinary rows.** They reconcile, attach, stop and
//! kill through the backend for their host exactly as local ones do, and
//! project groups are scoped by that host. A host that cannot be reached costs
//! its own error line and leaves its rows showing their last known status —
//! never the whole tree.
//!
//! **A workspace this client has only *seen* is a row too.** The store's view
//! is the rows plus what the hosts hold beyond them; a discovered one is dimmed
//! and says "not opened here", and its menu offers Open alone — opening is what
//! persists it, and nothing else on this panel may.

use crate::{
    AdeWorkspace, AdeWorkspaceStore, DaemonKey, SessionState, WorkspaceEntry, WorkspaceGone,
    WorkspaceId, WorkspaceLifecycleService, WorkspaceStatus,
    create_workspace_modal::CreateWorkspaceModal, lifecycle::display_name_for,
    open_workspace_session, project_id_from_path,
};
use anyhow::Result;
use gpui::{Entity, EventEmitter, FocusHandle, Focusable, WeakEntity, actions};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::Path,
    rc::Rc,
    sync::Arc,
};
use ui::{
    ContextMenu, Divider, Indicator, ListItem, ListItemSpacing, SpinnerLabel, Tooltip, prelude::*,
    right_click_menu,
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, TabContentParams},
};

actions!(
    ade_workspaces,
    [
        /// Toggles the ADE workspaces debug view.
        ToggleWorkspacesView,
    ]
);

/// One rendered line of the tree. Kept separate from the render so the
/// grouping is testable without a window.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SidebarRow {
    Project {
        project_id: String,
        remote_host: Option<String>,
        live_count: usize,
        gone_count: usize,
    },
    Workspace(WorkspaceEntry),
}

/// What a selected row is remembered by.
///
/// Daemon plus wire id, so opening a discovered workspace — which mints or
/// promotes a row for it — neither deselects it nor makes it flicker: the key
/// it was clicked under is the key the row carries, even when the row is filed
/// under another spelling of the same host. The row uuid is only for a
/// persisted row whose daemon record was killed, which has no wire id left to
/// be known by.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SelectionKey {
    Wire(DaemonKey, String),
    Row(WorkspaceId),
}

/// Which host spellings the last pass found to be one daemon — see
/// [`crate::Reconciled::daemons`].
type Daemons = HashMap<Option<String>, String>;

fn daemon_of(daemons: &Daemons, host: Option<&str>) -> DaemonKey {
    match daemons.get(&host.map(str::to_owned)) {
        Some(instance) => DaemonKey::Instance(instance.clone()),
        None => DaemonKey::Host(host.map(str::to_owned)),
    }
}

fn selection_key(daemons: &Daemons, entry: &WorkspaceEntry) -> SelectionKey {
    match entry {
        WorkspaceEntry::Persisted(workspace, _) => persisted_key(daemons, workspace),
        WorkspaceEntry::Discovered {
            remote_host,
            workspace,
            ..
        } => SelectionKey::Wire(
            daemon_of(daemons, remote_host.as_deref()),
            workspace.id.clone(),
        ),
    }
}

fn persisted_key(daemons: &Daemons, workspace: &AdeWorkspace) -> SelectionKey {
    match workspace.daemon_workspace_id() {
        Some(wire_id) => SelectionKey::Wire(
            daemon_of(daemons, workspace.remote_host.as_deref()),
            wire_id.to_owned(),
        ),
        None => SelectionKey::Row(workspace.id.clone()),
    }
}

/// The project a row groups under. A discovered workspace has no row to read it
/// off, so it comes from the root the host reported.
fn entry_project_id(entry: &WorkspaceEntry) -> String {
    match entry {
        WorkspaceEntry::Persisted(workspace, _) => workspace.project_id.clone(),
        WorkspaceEntry::Discovered { workspace, .. } => {
            project_id_from_path(Path::new(&workspace.project_root))
        }
    }
}

/// The registry row behind a row's context menu, and the gate on the
/// destructive half of it: stop, rename and delete each take a [`WorkspaceId`]
/// a discovered workspace has not got, and offering them would persist a row
/// for something that is not a use.
fn menu_row(entry: &WorkspaceEntry) -> Option<&AdeWorkspace> {
    entry.persisted().map(|(workspace, _)| workspace)
}

/// Drops the stale half of a (daemon, wire id) pair: a snapshot taken across a
/// promotion can hold both the new row and the discovery it came from. The row
/// wins — it is the same workspace, plus branch and recency.
///
/// By daemon rather than by spelling, because the row a promotion confirms may
/// be one that was quarantined under another alias of the same host.
fn dedupe_entries(entries: &[WorkspaceEntry], daemons: &Daemons) -> Vec<WorkspaceEntry> {
    let mut seen: HashSet<(DaemonKey, &str)> = HashSet::new();
    for entry in entries {
        if let (WorkspaceEntry::Persisted(..), Some(wire_id)) = (entry, entry.wire_id()) {
            seen.insert((daemon_of(daemons, entry.remote_host()), wire_id));
        }
    }
    entries
        .iter()
        .filter(|entry| match (entry, entry.wire_id()) {
            (WorkspaceEntry::Discovered { .. }, Some(wire_id)) => {
                !seen.contains(&(daemon_of(daemons, entry.remote_host()), wire_id))
            }
            _ => true,
        })
        .cloned()
        .collect()
}

/// Flattens reconciled workspaces into project-grouped rows.
///
/// Group order is first-appearance, and within a group the input order is
/// preserved — the registry lists most-recently-opened first and discovered
/// workspaces follow in the deterministic order reconciliation put them in, so
/// the project you last worked in floats to the top and its newest workspace
/// leads it.
///
/// The counts are of the entries actually in the group, so a host that could
/// not be reached — whose entries all read [`SessionState::Unknown`], counting
/// neither live nor gone — keeps its rows and its heading rather than emptying.
pub(crate) fn group_rows(entries: &[WorkspaceEntry]) -> Vec<SidebarRow> {
    let mut order: Vec<(Option<String>, String)> = Vec::new();
    let mut grouped: HashMap<(Option<String>, String), Vec<&WorkspaceEntry>> = HashMap::new();

    for entry in entries {
        let key = (
            entry.remote_host().map(ToOwned::to_owned),
            entry_project_id(entry),
        );
        grouped
            .entry(key.clone())
            .or_insert_with(|| {
                order.push(key);
                Vec::new()
            })
            .push(entry);
    }

    let mut rows = Vec::with_capacity(entries.len() + order.len());
    for key in order {
        let group = grouped.remove(&key).unwrap_or_default();
        let live_count = group
            .iter()
            .filter(|entry| entry.state() == SessionState::Alive)
            .count();
        let gone_count = group
            .iter()
            .filter(|entry| entry.state() == SessionState::Dead)
            .count();
        let (remote_host, project_id) = key;
        rows.push(SidebarRow::Project {
            project_id,
            remote_host,
            live_count,
            gone_count,
        });
        rows.extend(
            group
                .into_iter()
                .map(|entry| SidebarRow::Workspace(entry.clone())),
        );
    }
    rows
}

/// The dot's colour, straight off the theme's status palette — the whole point
/// of the panel is that this mapping is never guessed at in a component.
pub(crate) fn status_color(status: WorkspaceStatus) -> Color {
    match status {
        WorkspaceStatus::Running => Color::Success,
        WorkspaceStatus::Disconnected => Color::Warning,
        WorkspaceStatus::Stopped | WorkspaceStatus::Creating => Color::Muted,
        WorkspaceStatus::Error => Color::Error,
    }
}

pub struct WorkspaceSidebar {
    workspace: WeakEntity<Workspace>,
    lifecycle: Arc<WorkspaceLifecycleService>,
    /// The shared view of what the session backend last reported. Observed, not
    /// polled — see the module docs.
    store: Entity<AdeWorkspaceStore>,
    focus_handle: FocusHandle,
    rows: Vec<SidebarRow>,
    /// The workspace whose terminal the centre is showing, as far as this
    /// panel knows.
    selected: Option<SelectionKey>,
    /// The spelling-to-daemon map of the pass the rows came from, so a
    /// selection keyed by daemon can be recomputed while rendering.
    daemons: Daemons,
    /// The last failed *action*, shown in the panel rather than swallowed — a
    /// session backend that will not start is exactly what the user needs to
    /// see. The store carries the failures of its own passes.
    error: Option<SharedString>,
    _store_observation: gpui::Subscription,
}

impl WorkspaceSidebar {
    fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let lifecycle = crate::lifecycle_service(cx);
        let store = AdeWorkspaceStore::global(cx);
        let store_observation = cx.observe(&store, |this, store, cx| {
            this.take_rows_from_store(&store, cx);
        });

        let mut this = Self {
            workspace,
            lifecycle,
            store: store.clone(),
            focus_handle: cx.focus_handle(),
            rows: Vec::new(),
            selected: None,
            daemons: Daemons::new(),
            error: None,
            _store_observation: store_observation,
        };
        this.take_rows_from_store(&store, cx);
        this
    }

    /// Rebuilds the tree from the store's entries. The only thing that ever
    /// writes [`Self::rows`].
    fn take_rows_from_store(&mut self, store: &Entity<AdeWorkspaceStore>, cx: &mut Context<Self>) {
        self.daemons = store.read(cx).daemons().clone();
        let entries = dedupe_entries(store.read(cx).entries(), &self.daemons);
        // A workspace that has been removed can no longer be the selection, or
        // the row would stay highlighted with nothing under it. Keyed by host
        // and wire id, so a discovery that has just become a row survives this.
        if let Some(selected) = &self.selected
            && !entries
                .iter()
                .any(|entry| &selection_key(&self.daemons, entry) == selected)
        {
            self.selected = None;
        }
        self.rows = group_rows(&entries);
        cx.notify();
    }

    /// Asks the store for a fresh pass over the session backend, so the dots
    /// catch up with whatever the last action did.
    fn reconcile(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| store.refresh(cx));
    }

    /// Runs one blocking lifecycle call on the background executor, records
    /// any failure, and reconciles so the dots catch up with what it did.
    fn run_action<F>(
        &mut self,
        cx: &mut Context<Self>,
        action: impl FnOnce(Arc<WorkspaceLifecycleService>) -> F + 'static,
    ) where
        F: Future<Output = Result<()>> + Send + 'static,
    {
        let lifecycle = self.lifecycle.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(action(lifecycle)).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.error = None,
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                this.reconcile(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Clicking a row. Selection is set from the row's own key before anything
    /// blocks, so it does not wait on a host — and because that key is host
    /// plus wire id, a discovered row stays selected across the promotion below.
    fn open_entry(&mut self, entry: WorkspaceEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(selection_key(&self.daemons, &entry));
        cx.notify();

        match entry {
            WorkspaceEntry::Persisted(workspace, _) => {
                self.select_workspace(workspace.id, window, cx)
            }
            WorkspaceEntry::Discovered {
                remote_host,
                workspace,
                ..
            } => self.open_discovered(remote_host, workspace.id, window, cx),
        }
    }

    /// Hands a row to the shared open path, which marks the workspace opened,
    /// probes its session, and attaches unless it is dead — see
    /// [`open_workspace_session`].
    fn select_workspace(&mut self, id: WorkspaceId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(zed_workspace) = self.workspace.upgrade() else {
            return;
        };
        let open = open_workspace_session(&zed_workspace, id, window, cx);
        cx.spawn(async move |this, cx| {
            let outcome = open.await;
            this.update(cx, |this, _| {
                this.error = outcome.err().map(|error| format!("{error:#}").into());
            })
            .ok();
        })
        .detach();
    }

    /// Opening a workspace this client has only ever *seen*: confirm the record
    /// first, then the ordinary open path with the row it produced.
    ///
    /// Using it is what persists it, so the confirmation happens here and
    /// nowhere on the render path. Everything downstream — `last_opened_at`,
    /// the layout install — therefore still only ever sees a registry row.
    fn open_discovered(
        &mut self,
        host: Option<String>,
        wire_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let lifecycle = self.lifecycle.clone();
        let store = self.store.clone();
        let zed_workspace = self.workspace.clone();
        let this = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                let confirmed = cx
                    .background_spawn({
                        let host = host.clone();
                        let wire_id = wire_id.clone();
                        async move {
                            lifecycle
                                .confirm_discovered(host.as_deref(), &wire_id)
                                .await
                        }
                    })
                    .await;

                let error = match confirmed {
                    Ok(row) => {
                        let open = cx
                            .update(|window, cx| {
                                let zed_workspace = zed_workspace.upgrade()?;
                                Some(open_workspace_session(&zed_workspace, row.id, window, cx))
                            })
                            .ok()
                            .flatten();
                        match open {
                            Some(open) => open.await.err(),
                            None => None,
                        }
                    }
                    Err(error) => {
                        // The record went between the listing this row was
                        // drawn from and the click. Not a broken host and not
                        // worth retrying: drop the entry.
                        if error.downcast_ref::<WorkspaceGone>().is_some() {
                            store.update(cx, |store, cx| {
                                store.forget_workspace(host.as_deref(), &wire_id, cx)
                            });
                        }
                        Some(error)
                    }
                };

                this.update(cx, |this, _| {
                    this.error = error.map(|error| format!("{error:#}").into());
                })
                .ok();
            })
            .detach();
    }

    /// Detaches every client and records the workspace stopped. **Nothing
    /// dies** — the agents in the session keep running, which is why the menu
    /// entry says "Stop (detach)".
    fn stop_workspace(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        self.run_action(cx, move |lifecycle| async move {
            lifecycle.stop_workspace(&id).await.map(|_| ())
        });
    }

    /// The one destructive control in this panel: kills every session in the
    /// workspace — a single [`WorkspaceLifecycleService::kill_workspace`],
    /// which is one daemon `KillWorkspace` — then forgets the row. Other
    /// clients showing this workspace are told and stop syncing.
    ///
    /// **Unconfirmed, by spec.** A workspace with no sessions is a normal row,
    /// not a state to recover from, so there is nothing to ask about.
    ///
    /// **A failed kill keeps the row.** Dropping it anyway reported success for
    /// an action that killed nothing — the agents kept running on an
    /// unreachable host — and the next mirror re-adopted the same record under
    /// a fresh uuid, so the workspace came back with its branch and its
    /// selection lost. Killing the record and dropping the row is one
    /// operation, [`WorkspaceLifecycleService::kill_workspace`], so the two
    /// cannot come apart here.
    ///
    /// The selection is not cleared here either: the store observation clears
    /// it when the row actually goes, and clearing it in front of an async kill
    /// deselects a workspace that is still there.
    fn delete_workspace(&mut self, workspace: &AdeWorkspace, cx: &mut Context<Self>) {
        // [`Self::run_action`] with one addition it has no other use for: the
        // selection follows the row, and only once the row is really gone.
        let lifecycle = self.lifecycle.clone();
        let deleted = persisted_key(&self.daemons, workspace);
        let id = workspace.id.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { lifecycle.kill_workspace(&id).await.map(|_| ()) })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.error = None;
                        if this.selected.as_ref() == Some(&deleted) {
                            this.selected = None;
                        }
                    }
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                this.reconcile(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Opens the rename modal, which asks the daemon and lets the mirror carry
    /// the new name to every other client.
    fn rename_workspace(
        &mut self,
        workspace: AdeWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(zed_workspace) = self.workspace.upgrade() else {
            return;
        };
        crate::open_rename_workspace_modal(&zed_workspace, workspace, window, cx);
    }

    fn create_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lifecycle = self.lifecycle.clone();
        let this = cx.entity().downgrade();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    CreateWorkspaceModal::new(
                        lifecycle,
                        Rc::new(move |created: AdeWorkspace, window, cx| {
                            this.update(cx, |this, cx| {
                                // Creating made the record and nothing else;
                                // selecting it is what opens its first terminal.
                                this.selected = Some(persisted_key(&this.daemons, &created));
                                this.select_workspace(created.id.clone(), window, cx);
                            })
                            .ok();
                        }),
                        window,
                        cx,
                    )
                });
            })
            .log_err();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .h(rems(1.75))
            .px_2()
            .justify_between()
            .child(Label::new("Workspaces"))
            .child(
                IconButton::new("ade-new-workspace", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .aria_label("New workspace")
                    .tooltip(Tooltip::text("New workspace"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.create_workspace(window, cx);
                    })),
            )
    }

    fn render_project_header(
        &self,
        ix: usize,
        project_id: &str,
        remote_host: Option<&str>,
        live_count: usize,
        gone_count: usize,
        contains_selection: bool,
        cx: &App,
    ) -> AnyElement {
        h_flex()
            .id(("ade-project-header", ix))
            .h(rems(1.75))
            .px_2()
            .justify_between()
            .child(
                h_flex()
                    .min_w_0()
                    .gap_1p5()
                    .child(Icon::new(IconName::Folder).size(IconSize::Small).color(
                        if contains_selection {
                            Color::Default
                        } else {
                            Color::Muted
                        },
                    ))
                    // The project is a human name, so it stays in the UI
                    // font; only machine identifiers below go mono.
                    .child(
                        Label::new(project_id.to_owned())
                            .color(if contains_selection {
                                Color::Default
                            } else {
                                Color::Muted
                            })
                            .truncate(),
                    )
                    .child(
                        Label::new(remote_host.unwrap_or("local").to_owned())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .buffer_font(cx)
                            .truncate(),
                    ),
            )
            .child(
                Label::new(format!("{live_count} live · {gone_count} gone"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_workspace_row(
        &self,
        ix: usize,
        entry: &WorkspaceEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_selected = self.selected.as_ref() == Some(&selection_key(&self.daemons, entry));
        let row = menu_row(entry).cloned();
        let is_discovered = row.is_none();
        let (name, branch, dot_color, detail) = match entry {
            WorkspaceEntry::Persisted(workspace, state) => (
                workspace.name.clone(),
                workspace.branch.clone(),
                status_color(workspace_status(workspace, *state)),
                format!(
                    // Debug view: an unlinked row is worth *seeing* as unlinked.
                    "{} · {}",
                    workspace.daemon_workspace_id().unwrap_or("no record"),
                    workspace.repository_path.to_string_lossy()
                ),
            ),
            WorkspaceEntry::Discovered {
                workspace, state, ..
            } => (
                display_name_for(workspace, &entry_project_id(entry)),
                // No branch: nothing has checked one out here, and the daemon
                // records a root, not a checkout state.
                None,
                status_color(discovered_status(*state)),
                format!(
                    "not opened here · {} · {}",
                    workspace.id, workspace.project_root
                ),
            ),
        };
        // The row's element trees outlive this call, so nothing borrowed from
        // `entry` or `cx` may be captured — take owned copies first.
        let this = cx.entity().downgrade();
        let entry_for_click = entry.clone();
        let entry_for_menu = entry.clone();

        let element = {
            let this_for_click = this.clone();
            let this_for_menu = this;
            right_click_menu::<ContextMenu>(("ade-workspace-row", ix))
                .trigger(move |_, _, cx| {
                    ListItem::new(("ade-workspace-item", ix))
                        .spacing(ListItemSpacing::Sparse)
                        .indent_level(1)
                        .toggle_state(is_selected)
                        .start_slot(Indicator::dot().color(dot_color))
                        .child(
                            v_flex()
                                .w_full()
                                .min_w_0()
                                .child(
                                    h_flex()
                                        .w_full()
                                        .min_w_0()
                                        .gap_1p5()
                                        .child(
                                            Label::new(name)
                                                .color(if is_selected {
                                                    Color::Accent
                                                } else if is_discovered {
                                                    // Dimmed: a workspace this
                                                    // client has only seen.
                                                    Color::Muted
                                                } else {
                                                    Color::Default
                                                })
                                                .truncate(),
                                        )
                                        .children(branch.map(|branch| {
                                            Label::new(branch)
                                                .size(LabelSize::Small)
                                                .color(Color::Muted)
                                                .buffer_font(cx)
                                                .truncate()
                                        })),
                                )
                                .child(
                                    Label::new(detail)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .buffer_font(cx)
                                        .truncate(),
                                ),
                        )
                        .on_click(move |_, window, cx| {
                            this_for_click
                                .update(cx, |this, cx| {
                                    this.open_entry(entry_for_click.clone(), window, cx)
                                })
                                .ok();
                        })
                })
                .menu(move |window, cx| {
                    let this = this_for_menu.clone();
                    let entry = entry_for_menu.clone();
                    let row = row.clone();
                    ContextMenu::build(window, cx, move |menu, _, _| {
                        let menu =
                            menu.entry(if row.is_some() { "Reconnect" } else { "Open" }, None, {
                                let this = this.clone();
                                let entry = entry.clone();
                                move |window, cx| {
                                    this.update(cx, |this, cx| {
                                        this.open_entry(entry.clone(), window, cx)
                                    })
                                    .ok();
                                }
                            });
                        // Everything below takes a `WorkspaceId`, so a
                        // discovered workspace gets none of it: opening is the
                        // only thing that may persist a row.
                        let Some(row) = row.clone() else {
                            return menu;
                        };
                        let id = row.id.clone();
                        menu
                            // Detaches. The session and everything in it survives.
                            .entry("Stop (detach)", None, {
                                let this = this.clone();
                                move |_, cx| {
                                    this.update(cx, |this, cx| this.stop_workspace(id.clone(), cx))
                                        .ok();
                                }
                            })
                            // The daemon owns the name, so this goes over the wire
                            // and comes back to every other client through the
                            // mirror — see `WorkspaceLifecycleService::rename_workspace`.
                            .entry("Rename Workspace…", None, {
                                let this = this.clone();
                                let for_rename = row.clone();
                                move |window, cx| {
                                    this.update(cx, |this, cx| {
                                        this.rename_workspace(for_rename.clone(), window, cx)
                                    })
                                    .ok();
                                }
                            })
                            .separator()
                            // The only destructive entry, and it says so: every
                            // session in the workspace dies with the row, no
                            // confirmation (spec ruling — a zero-session row is
                            // normal, not a state to recover from).
                            .entry("Delete Workspace", None, {
                                move |_, cx| {
                                    this.update(cx, |this, cx| this.delete_workspace(&row, cx))
                                        .ok();
                                }
                            })
                    })
                })
        };

        // A dead session's dot already reads disconnected (see
        // `workspace_status` below); the row itself needs no special casing —
        // a workspace with no live session is a normal row.
        element.into_any_element()
    }

    fn render_empty_state(&self) -> AnyElement {
        v_flex()
            .p_3()
            .gap_1()
            .child(Label::new("No workspaces yet").color(Color::Muted))
            .child(
                Label::new("Use + to create one from a repository path.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }
}

/// The status the dot should show, which is the reconciled status except that
/// a session the backend says is dead reads as disconnected even if the
/// registry has not been written yet.
fn workspace_status(workspace: &AdeWorkspace, state: SessionState) -> WorkspaceStatus {
    match state {
        SessionState::Dead => WorkspaceStatus::Disconnected,
        _ => workspace.status,
    }
}

/// The dot for a discovered workspace, which has no registry status to fall
/// back on: what the host said about its session is all there is.
fn discovered_status(state: SessionState) -> WorkspaceStatus {
    match state {
        SessionState::Alive => WorkspaceStatus::Running,
        SessionState::Dead => WorkspaceStatus::Disconnected,
        // No session yet, or a host that could not be asked: nothing claimed.
        SessionState::NeverCreated | SessionState::Unknown => WorkspaceStatus::Stopped,
    }
}

impl Render for WorkspaceSidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows.clone();
        let selected_group = self.rows.iter().find_map(|row| match row {
            SidebarRow::Workspace(entry)
                if self.selected.as_ref() == Some(&selection_key(&self.daemons, entry)) =>
            {
                Some((
                    entry.remote_host().map(ToOwned::to_owned),
                    entry_project_id(entry),
                ))
            }
            _ => None,
        });
        let is_refreshing = self.store.read(cx).is_refreshing();

        v_flex()
            .key_context("AdeWorkspaceSidebar")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .child(Divider::horizontal())
            .children(
                {
                    let store = self.store.read(cx);
                    store
                        .status_stream_error()
                        .cloned()
                        .into_iter()
                        .chain(store.host_errors().iter().cloned())
                        .chain(store.error().cloned())
                        .chain(self.error.clone())
                        .collect::<Vec<_>>()
                }
                .into_iter()
                .map(|error| {
                    div()
                        .px_2()
                        .py_1()
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                }),
            )
            .child(
                v_flex()
                    .id("ade-workspace-tree")
                    .flex_1()
                    .py_1()
                    .overflow_y_scroll()
                    .map(|this| {
                        if rows.is_empty() {
                            if is_refreshing {
                                return this;
                            }
                            return this.child(self.render_empty_state());
                        }
                        this.children(rows.iter().enumerate().map(|(ix, row)| match row {
                            SidebarRow::Project {
                                project_id,
                                remote_host,
                                live_count,
                                gone_count,
                            } => self.render_project_header(
                                ix,
                                project_id,
                                remote_host.as_deref(),
                                *live_count,
                                *gone_count,
                                selected_group.as_ref().is_some_and(
                                    |(selected_host, selected_project)| {
                                        selected_host == remote_host
                                            && selected_project == project_id
                                    },
                                ),
                                cx,
                            ),
                            SidebarRow::Workspace(entry) => {
                                self.render_workspace_row(ix, entry, cx)
                            }
                        }))
                    }),
            )
    }
}

impl Focusable for WorkspaceSidebar {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for WorkspaceSidebar {}

/// A plain pane item, and **deliberately not a `SerializableItem`**: the view
/// is a debugging aid, so it must not be restored into a window on the next
/// launch — the only way back is the command palette.
impl Item for WorkspaceSidebar {
    type Event = ();

    fn to_item_events(_: &Self::Event, _: &mut dyn FnMut(workspace::item::ItemEvent)) {}

    fn tab_content(&self, params: TabContentParams, _: &Window, cx: &App) -> AnyElement {
        h_flex()
            .gap_1()
            .when(self.store.read(cx).is_refreshing(), |this| {
                this.child(SpinnerLabel::new())
            })
            .child(Label::new("Workspaces").color(params.text_color()))
            .into_any_element()
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Workspaces".into()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleWorkspacesView, window, cx| {
            // Toggle, not activate: a second invocation closes the tab, so the
            // view is never left resident.
            let existing = workspace.panes().iter().find_map(|pane| {
                let item = pane.read(cx).items_of_type::<WorkspaceSidebar>().next()?;
                Some((pane.clone(), item.entity_id()))
            });
            if let Some((pane, item_id)) = existing {
                pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(item_id, workspace::SaveIntent::Close, window, cx)
                })
                .detach_and_log_err(cx);
                return;
            }

            let handle = cx.entity().downgrade();
            let sidebar = cx.new(|cx| WorkspaceSidebar::new(handle, cx));
            workspace.add_item_to_active_pane(Box::new(sidebar), None, true, window, cx);
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AdeWorkspaceRegistry, Attached, BackendWorkspace, DaemonRefusal, SessionBackend, SessionId,
        SessionInfo, SessionSpec, StatusDelivery,
    };
    use ade_session::proto;
    use gpui::{Subscription, TestAppContext};
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
    };
    use time::{Duration, OffsetDateTime};

    struct PromptBackend;

    impl SessionBackend for PromptBackend {
        fn create(&self, _: &SessionSpec) -> Result<SessionId> {
            anyhow::bail!("unexpected create")
        }

        fn list(&self) -> Result<Vec<SessionInfo>> {
            Ok(Vec::new())
        }

        fn exists(&self, _: &SessionId) -> Result<bool> {
            Ok(false)
        }

        fn attach(&self, _: &SessionSpec) -> Result<Attached> {
            anyhow::bail!("unexpected attach")
        }

        fn detach(&self, _: &SessionId) -> Result<()> {
            Ok(())
        }

        fn kill(&self, _: &SessionId) -> Result<()> {
            Ok(())
        }

        fn status_delivery(&self) -> StatusDelivery {
            StatusDelivery::Push
        }
    }

    fn entry(
        name: &str,
        project_id: &str,
        status: WorkspaceStatus,
        state: SessionState,
    ) -> WorkspaceEntry {
        let mut workspace = AdeWorkspace::new(name, project_id, PathBuf::from("/repos/zed"));
        workspace.status = status;
        WorkspaceEntry::Persisted(workspace, state)
    }

    fn row_of(entry: &WorkspaceEntry) -> &AdeWorkspace {
        entry.persisted().expect("a persisted entry").0
    }

    fn discovered(wire_id: &str, root: &str, state: SessionState) -> WorkspaceEntry {
        WorkspaceEntry::Discovered {
            remote_host: None,
            workspace: BackendWorkspace {
                id: wire_id.to_owned(),
                name: "on the host".to_owned(),
                project_root: root.to_owned(),
                created_at: 1_700_000_000,
            },
            state,
        }
    }

    #[test]
    fn test_group_rows_preserves_order_and_counts() {
        let entries = vec![
            entry("main", "zed", WorkspaceStatus::Running, SessionState::Alive),
            entry(
                "elsewhere",
                "praxis",
                WorkspaceStatus::Stopped,
                SessionState::NeverCreated,
            ),
            // Back to the first project: it must rejoin its own group rather
            // than open a second heading.
            entry(
                "spike",
                "zed",
                WorkspaceStatus::Disconnected,
                SessionState::Dead,
            ),
        ];

        let rows = group_rows(&entries);
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[0],
            SidebarRow::Project {
                project_id: "zed".into(),
                remote_host: None,
                live_count: 1,
                gone_count: 1,
            }
        );
        assert_eq!(rows[1], SidebarRow::Workspace(entries[0].clone()));
        assert_eq!(rows[2], SidebarRow::Workspace(entries[2].clone()));
        // The second project keeps first-appearance order, so it follows.
        assert_eq!(
            rows[3],
            SidebarRow::Project {
                project_id: "praxis".into(),
                remote_host: None,
                live_count: 0,
                gone_count: 0,
            }
        );
        assert_eq!(rows[4], SidebarRow::Workspace(entries[1].clone()));
    }

    #[test]
    fn test_group_rows_of_nothing_is_nothing() {
        assert!(group_rows(&[]).is_empty());
    }

    #[test]
    fn test_group_rows_scopes_projects_by_host() {
        let on_host = |name: &str, host: &str, status, state| {
            let mut entry = entry(name, "viral-studio", status, state);
            let WorkspaceEntry::Persisted(workspace, _) = &mut entry else {
                unreachable!()
            };
            workspace.remote_host = Some(host.to_owned());
            entry
        };
        let entries = vec![
            on_host(
                "first",
                "user@host-a",
                WorkspaceStatus::Running,
                SessionState::Alive,
            ),
            on_host(
                "other-host",
                "user@host-b",
                WorkspaceStatus::Running,
                SessionState::Alive,
            ),
            on_host(
                "second",
                "user@host-a",
                WorkspaceStatus::Disconnected,
                SessionState::Dead,
            ),
        ];

        let rows = group_rows(&entries);
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[0],
            SidebarRow::Project {
                project_id: "viral-studio".into(),
                remote_host: Some("user@host-a".into()),
                live_count: 1,
                gone_count: 1,
            }
        );
        assert_eq!(
            rows[3],
            SidebarRow::Project {
                project_id: "viral-studio".into(),
                remote_host: Some("user@host-b".into()),
                live_count: 1,
                gone_count: 0,
            }
        );
    }

    #[gpui::test]
    async fn test_store_reports_its_initial_refresh(cx: &mut TestAppContext) {
        let registry = AdeWorkspaceRegistry::open_test_db("test_store_initial_refresh").await;
        let lifecycle = Arc::new(WorkspaceLifecycleService::with_backend(
            registry,
            Arc::new(PromptBackend),
        ));
        cx.update(|cx| cx.set_global(crate::GlobalLifecycleService(lifecycle)));

        let store = cx.update(AdeWorkspaceStore::global);
        assert!(store.read_with(cx, |store, _| store.is_refreshing()));
        cx.run_until_parked();
        assert!(!store.read_with(cx, |store, _| store.is_refreshing()));
    }

    /// Records `kill_workspace` calls, so a test can tell the destructive path
    /// was actually taken rather than merely not erroring.
    ///
    /// Holds the one workspace it stands for so `list_workspaces` reports it —
    /// same as a real daemon would — which is what keeps a background
    /// reconcile pass from reading "nothing listed" as "nothing backs it" and
    /// dropping the row before the test's own delete runs.
    struct DeleteBackend {
        kill_workspace_calls: Mutex<Vec<String>>,
        workspaces: Mutex<Vec<BackendWorkspace>>,
        /// What the daemon answers a `KillWorkspace` with, when it refuses.
        refusal: Option<DaemonRefusal>,
    }

    impl DeleteBackend {
        fn new() -> Self {
            Self {
                kill_workspace_calls: Mutex::new(Vec::new()),
                workspaces: Mutex::new(vec![BackendWorkspace {
                    id: "ws-zed-1".to_owned(),
                    name: "main".to_owned(),
                    project_root: "/repos/zed".to_owned(),
                    created_at: 1_700_000_000,
                }]),
                refusal: None,
            }
        }

        /// A daemon that refuses the kill with `code`, keeping its record — the
        /// unreachable host, and the kill that happened but was not recorded.
        fn refusing(code: &str) -> Self {
            Self {
                refusal: Some(DaemonRefusal {
                    code: code.to_owned(),
                    message: "the daemon said no".to_owned(),
                }),
                ..Self::new()
            }
        }
    }

    impl SessionBackend for DeleteBackend {
        fn create(&self, spec: &SessionSpec) -> Result<SessionId> {
            Ok(spec.id.clone())
        }

        fn list(&self) -> Result<Vec<SessionInfo>> {
            Ok(Vec::new())
        }

        fn exists(&self, _: &SessionId) -> Result<bool> {
            Ok(true)
        }

        fn attach(&self, spec: &SessionSpec) -> Result<Attached> {
            Ok(Attached {
                session_id: spec.id.to_string(),
                argv: vec!["attach".to_owned()],
            })
        }

        fn detach(&self, _: &SessionId) -> Result<()> {
            Ok(())
        }

        fn kill(&self, _: &SessionId) -> Result<()> {
            Ok(())
        }

        fn status_delivery(&self) -> StatusDelivery {
            StatusDelivery::Push
        }

        fn kill_workspace(&self, workspace_id: &str) -> Result<()> {
            self.kill_workspace_calls
                .lock()
                .unwrap()
                .push(workspace_id.to_owned());
            if let Some(refusal) = self.refusal.clone() {
                return Err(anyhow::Error::new(refusal));
            }
            self.workspaces
                .lock()
                .unwrap()
                .retain(|workspace| workspace.id != workspace_id);
            Ok(())
        }

        fn list_workspaces(&self) -> Result<crate::WorkspaceListing> {
            Ok(crate::WorkspaceListing {
                workspaces: self.workspaces.lock().unwrap().clone(),
                degraded: false,
            })
        }
    }

    /// One sidebar showing one workspace `backend` holds, ready to be deleted.
    #[allow(clippy::type_complexity)]
    async fn delete_fixture<'a>(
        name: &'static str,
        backend: DeleteBackend,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<WorkspaceSidebar>,
        Arc<DeleteBackend>,
        Arc<WorkspaceLifecycleService>,
        AdeWorkspace,
        &'a mut gpui::VisualTestContext,
    ) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let registry = AdeWorkspaceRegistry::open_test_db(name).await;
        let mut workspace = AdeWorkspace::new("main", "zed", PathBuf::from("/repos/zed"));
        workspace.terminal_session_id = Some("ws-zed-1".to_owned());
        registry
            .create_workspace(workspace.clone())
            .await
            .expect("workspace should be registered");

        let backend = Arc::new(backend);
        let lifecycle = Arc::new(WorkspaceLifecycleService::with_backend(
            registry,
            backend.clone(),
        ));
        cx.update(|cx| cx.set_global(crate::GlobalLifecycleService(lifecycle.clone())));

        let rows = group_rows(&[WorkspaceEntry::Persisted(
            workspace.clone(),
            SessionState::Alive,
        )]);
        let selected = Some(persisted_key(&Daemons::new(), &workspace));
        let (sidebar, cx) = cx.add_window_view(|_, cx| WorkspaceSidebar {
            workspace: WeakEntity::new_invalid(),
            lifecycle: lifecycle.clone(),
            store: AdeWorkspaceStore::global(cx),
            focus_handle: cx.focus_handle(),
            rows,
            selected,
            daemons: Daemons::new(),
            error: None,
            _store_observation: Subscription::new(|| {}),
        });
        (sidebar, backend, lifecycle, workspace, cx)
    }

    /// "Delete Workspace" is the one destructive entry, and it is unconfirmed:
    /// no prompt appears, the daemon record and its sessions are killed with a
    /// single `KillWorkspace`, and the registry forgets the row.
    #[gpui::test]
    async fn test_delete_workspace_kills_and_forgets_the_row_without_confirmation(
        cx: &mut TestAppContext,
    ) {
        let (sidebar, backend, lifecycle, workspace, cx) = delete_fixture(
            "test_delete_workspace_no_confirmation",
            DeleteBackend::new(),
            cx,
        )
        .await;

        sidebar.update(cx, |sidebar, cx| {
            sidebar.delete_workspace(&workspace, cx);
        });
        cx.run_until_parked();

        assert!(
            !cx.has_pending_prompt(),
            "row delete must not ask for confirmation"
        );
        assert_eq!(
            backend.kill_workspace_calls.lock().unwrap().as_slice(),
            [workspace.daemon_workspace_id().unwrap().to_owned()]
        );
        assert_eq!(
            lifecycle
                .registry()
                .get_workspace(workspace.id.clone())
                .unwrap(),
            None,
            "the registry must forget a deleted row"
        );
        sidebar.read_with(cx, |sidebar, _| {
            assert!(
                sidebar.selected.is_none(),
                "a deleted row cannot stay selected"
            );
        });
    }

    /// A kill the daemon refused leaves the row, the selection and the sessions
    /// exactly where they were, and says so in the panel.
    ///
    /// Deleting the row anyway reported success for an action that killed
    /// nothing — agents still running on an unreachable host — and the next
    /// mirror brought the same workspace back under a fresh uuid.
    #[gpui::test]
    async fn test_a_refused_delete_keeps_the_row_and_surfaces_the_failure(cx: &mut TestAppContext) {
        let (sidebar, backend, lifecycle, workspace, cx) = delete_fixture(
            "test_delete_workspace_refused",
            DeleteBackend::refusing(proto::error_code::INTERNAL),
            cx,
        )
        .await;

        sidebar.update(cx, |sidebar, cx| {
            sidebar.delete_workspace(&workspace, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            backend.kill_workspace_calls.lock().unwrap().len(),
            1,
            "the kill is still attempted"
        );
        assert_eq!(
            lifecycle
                .registry()
                .get_workspace(workspace.id.clone())
                .unwrap()
                .map(|row| row.id),
            Some(workspace.id.clone()),
            "a failed kill keeps the row, uuid and all"
        );
        sidebar.read_with(cx, |sidebar, _| {
            assert!(
                sidebar
                    .error
                    .as_ref()
                    .is_some_and(|error| error.contains("internal")),
                "the panel shows the daemon's refusal: {:?}",
                sidebar.error
            );
            assert_eq!(
                sidebar.selected,
                Some(persisted_key(&Daemons::new(), &workspace)),
                "a row that is still there stays selected"
            );
        });
    }

    /// `persist_failed` is the one refusal that still deletes the row: the
    /// sessions *were* killed and only the daemon's ledger did not take it, so
    /// the row addresses nothing.
    #[gpui::test]
    async fn test_a_kill_that_could_not_be_recorded_still_forgets_the_row(cx: &mut TestAppContext) {
        let (sidebar, _backend, lifecycle, workspace, cx) = delete_fixture(
            "test_delete_workspace_persist_failed",
            DeleteBackend::refusing(proto::error_code::PERSIST_FAILED),
            cx,
        )
        .await;

        sidebar.update(cx, |sidebar, cx| {
            sidebar.delete_workspace(&workspace, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            lifecycle
                .registry()
                .get_workspace(workspace.id.clone())
                .unwrap(),
            None,
            "the kill happened, so the row goes"
        );
        sidebar.read_with(cx, |sidebar, _| assert!(sidebar.error.is_none()));
    }

    /// A workspace with no session is a normal row: it groups and lists next
    /// to a live one exactly the same way, with no filtering and no special
    /// "gone" wrapper — the "no gone state" half of the spec ruling.
    #[test]
    fn test_a_zero_session_workspace_lists_as_a_plain_row() {
        let alive = entry("main", "zed", WorkspaceStatus::Running, SessionState::Alive);
        let empty = entry(
            "scratch",
            "zed",
            WorkspaceStatus::Creating,
            SessionState::NeverCreated,
        );
        let rows = group_rows(&[alive.clone(), empty.clone()]);
        assert_eq!(
            rows,
            vec![
                SidebarRow::Project {
                    project_id: "zed".into(),
                    remote_host: None,
                    live_count: 1,
                    gone_count: 0,
                },
                SidebarRow::Workspace(alive),
                SidebarRow::Workspace(empty),
            ]
        );
    }

    #[test]
    fn test_status_colors_come_from_the_status_palette() {
        assert_eq!(status_color(WorkspaceStatus::Running), Color::Success);
        assert_eq!(status_color(WorkspaceStatus::Disconnected), Color::Warning);
        assert_eq!(status_color(WorkspaceStatus::Stopped), Color::Muted);
        assert_eq!(status_color(WorkspaceStatus::Creating), Color::Muted);
        assert_eq!(status_color(WorkspaceStatus::Error), Color::Error);
    }

    #[test]
    fn test_a_dead_session_reads_disconnected_before_the_registry_catches_up() {
        let mut workspace = row_of(&entry(
            "main",
            "zed",
            WorkspaceStatus::Running,
            SessionState::Alive,
        ))
        .clone();
        // The backend says gone; the row must not still show green.
        assert_eq!(
            workspace_status(&workspace, SessionState::Dead),
            WorkspaceStatus::Disconnected
        );
        assert_eq!(
            status_color(workspace_status(&workspace, SessionState::Dead)),
            Color::Warning
        );
        // Otherwise the reconciled status is taken as-is.
        assert_eq!(
            workspace_status(&workspace, SessionState::Alive),
            WorkspaceStatus::Running
        );
        workspace.status = WorkspaceStatus::Error;
        assert_eq!(
            workspace_status(&workspace, SessionState::NeverCreated),
            WorkspaceStatus::Error
        );
    }

    /// A workspace a host holds that this client has never opened is a row like
    /// any other: it groups under its own project and its session counts.
    #[test]
    fn test_discovered_entries_group_and_count_like_rows() {
        let entries = vec![
            entry("main", "zed", WorkspaceStatus::Running, SessionState::Alive),
            discovered("ws-praxis-1", "/repos/praxis", SessionState::Alive),
            discovered("ws-zed-9", "/repos/zed", SessionState::Dead),
        ];

        let rows = group_rows(&entries);
        assert_eq!(
            rows,
            vec![
                SidebarRow::Project {
                    project_id: "zed".into(),
                    remote_host: None,
                    live_count: 1,
                    gone_count: 1,
                },
                SidebarRow::Workspace(entries[0].clone()),
                SidebarRow::Workspace(entries[2].clone()),
                SidebarRow::Project {
                    project_id: "praxis".into(),
                    remote_host: None,
                    live_count: 1,
                    gone_count: 0,
                },
                SidebarRow::Workspace(entries[1].clone()),
            ]
        );
    }

    /// A snapshot taken across a promotion holds both the new row and the
    /// discovery it came from. One row is drawn, and it is the persisted one.
    #[test]
    fn test_a_row_wins_over_the_discovery_it_was_promoted_from() {
        let mut workspace = AdeWorkspace::new("main", "zed", PathBuf::from("/repos/zed"));
        workspace.terminal_session_id = Some("ws-zed-1".to_owned());
        let persisted = WorkspaceEntry::Persisted(workspace, SessionState::Alive);
        let stale = discovered("ws-zed-1", "/repos/zed", SessionState::Alive);

        for snapshot in [
            [persisted.clone(), stale.clone()],
            [stale, persisted.clone()],
        ] {
            assert_eq!(
                dedupe_entries(&snapshot, &Daemons::new()),
                vec![persisted.clone()]
            );
        }
        // A different wire id is a different workspace, not a duplicate.
        assert_eq!(
            dedupe_entries(
                &[
                    persisted,
                    discovered("ws-zed-2", "/repos/zed", SessionState::Alive)
                ],
                &Daemons::new()
            )
            .len(),
            2
        );
    }

    /// The key a discovered row is selected under is the key the row minted for
    /// it carries, so opening it neither deselects nor flickers it.
    #[test]
    fn test_selection_survives_the_promotion_an_open_performs() {
        let before = discovered("ws-zed-1", "/repos/zed", SessionState::NeverCreated);
        let mut row = AdeWorkspace::new("main", "zed", PathBuf::from("/repos/zed"));
        row.terminal_session_id = Some("ws-zed-1".to_owned());
        let daemons = Daemons::new();
        assert_eq!(
            selection_key(&daemons, &before),
            selection_key(
                &daemons,
                &WorkspaceEntry::Persisted(row, SessionState::Alive)
            )
        );

        // Wire ids are host-scoped, so the key is too.
        let WorkspaceEntry::Discovered { workspace, .. } = before.clone() else {
            unreachable!()
        };
        assert_ne!(
            selection_key(&daemons, &before),
            selection_key(
                &daemons,
                &WorkspaceEntry::Discovered {
                    remote_host: Some("user@host-b".into()),
                    workspace,
                    state: SessionState::NeverCreated,
                }
            )
        );
    }

    /// A host that could not be reached keeps its entries: `Unknown` counts
    /// neither live nor gone, so the group reads `0 live · 0 gone` rather than
    /// vanishing.
    #[test]
    fn test_an_unreachable_hosts_entries_count_neither_live_nor_gone() {
        let rows = group_rows(&[
            entry(
                "main",
                "zed",
                WorkspaceStatus::Running,
                SessionState::Unknown,
            ),
            discovered("ws-zed-9", "/repos/zed", SessionState::Unknown),
        ]);
        assert_eq!(
            rows[0],
            SidebarRow::Project {
                project_id: "zed".into(),
                remote_host: None,
                live_count: 0,
                gone_count: 0,
            }
        );
        assert_eq!(rows.len(), 3, "the entries themselves stay on screen");
    }

    /// A discovered workspace's menu is open-only: stop, rename and delete all
    /// take a registry row it has not got.
    #[test]
    fn test_a_discovered_row_offers_no_destructive_action() {
        assert!(menu_row(&discovered("ws-zed-9", "/repos/zed", SessionState::Alive)).is_none());
        assert!(
            menu_row(&entry(
                "main",
                "zed",
                WorkspaceStatus::Running,
                SessionState::Alive
            ))
            .is_some()
        );
    }

    /// One sidebar over an empty registry, with `backend` holding whatever this
    /// client has never opened.
    #[allow(clippy::type_complexity)]
    async fn discovery_fixture<'a>(
        name: &'static str,
        backend: DeleteBackend,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<WorkspaceSidebar>,
        Arc<DeleteBackend>,
        Arc<WorkspaceLifecycleService>,
        &'a mut gpui::VisualTestContext,
    ) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let registry = AdeWorkspaceRegistry::open_test_db(name).await;
        let backend = Arc::new(backend);
        let lifecycle = Arc::new(WorkspaceLifecycleService::with_backend(
            registry,
            backend.clone(),
        ));
        cx.update(|cx| cx.set_global(crate::GlobalLifecycleService(lifecycle.clone())));

        let (sidebar, cx) = cx.add_window_view(|_, cx| WorkspaceSidebar {
            workspace: WeakEntity::new_invalid(),
            lifecycle: lifecycle.clone(),
            store: AdeWorkspaceStore::global(cx),
            focus_handle: cx.focus_handle(),
            rows: Vec::new(),
            selected: None,
            daemons: Daemons::new(),
            error: None,
            _store_observation: Subscription::new(|| {}),
        });
        (sidebar, backend, lifecycle, cx)
    }

    /// Opening a discovered workspace is what persists it: one confirmation,
    /// one row, `last_opened_at` stamped now rather than at the daemon's
    /// creation time. Clicking twice is not two workspaces.
    #[gpui::test]
    async fn test_opening_a_discovered_workspace_confirms_it_once(cx: &mut TestAppContext) {
        let (sidebar, _backend, lifecycle, cx) = discovery_fixture(
            "test_open_discovered_confirms_once",
            DeleteBackend::new(),
            cx,
        )
        .await;

        let entry = discovered("ws-zed-1", "/repos/zed", SessionState::NeverCreated);
        for _ in 0..2 {
            cx.update(|window, cx| {
                sidebar.update(cx, |sidebar, cx| {
                    sidebar.open_entry(entry.clone(), window, cx)
                })
            });
            cx.run_until_parked();
        }

        let rows = lifecycle.registry().list_workspaces().unwrap();
        assert_eq!(rows.len(), 1, "clicking twice is not two workspaces");
        assert_eq!(rows[0].terminal_session_id.as_deref(), Some("ws-zed-1"));
        assert!(
            (OffsetDateTime::now_utc() - rows[0].last_opened_at).abs() < Duration::minutes(1),
            "opening stamps now, not the daemon's creation time: {:?}",
            rows[0].last_opened_at
        );
        sidebar.read_with(cx, |sidebar, _| {
            assert_eq!(
                sidebar.selected,
                Some(selection_key(
                    &sidebar.daemons,
                    &WorkspaceEntry::Persisted(rows[0].clone(), SessionState::Alive)
                )),
                "the row it became answers to the key it was clicked under"
            );
        });
    }

    /// The record went between the listing the row was drawn from and the
    /// click. The entry is dropped rather than left addressing nothing, and the
    /// panel says what happened.
    #[gpui::test]
    async fn test_opening_a_vanished_discovery_drops_it(cx: &mut TestAppContext) {
        let (sidebar, backend, lifecycle, cx) =
            discovery_fixture("test_open_discovered_gone", DeleteBackend::new(), cx).await;
        cx.run_until_parked();
        let store = cx.update(|_, cx| AdeWorkspaceStore::global(cx));
        assert_eq!(
            store.read_with(cx, |store, _| store.entries().len()),
            1,
            "the host's record is discovered before it goes"
        );

        backend.workspaces.lock().unwrap().clear();
        cx.update(|window, cx| {
            sidebar.update(cx, |sidebar, cx| {
                sidebar.open_entry(
                    discovered("ws-zed-1", "/repos/zed", SessionState::NeverCreated),
                    window,
                    cx,
                )
            })
        });
        cx.run_until_parked();

        assert!(
            lifecycle.registry().list_workspaces().unwrap().is_empty(),
            "a record that is gone is never persisted"
        );
        assert!(
            store.read_with(cx, |store, _| store.entries().is_empty()),
            "the entry goes with it"
        );
        sidebar.read_with(cx, |sidebar, _| {
            assert!(
                sidebar
                    .error
                    .as_ref()
                    .is_some_and(|error| error.contains("no longer holds")),
                "the panel says the record is gone: {:?}",
                sidebar.error
            );
        });
    }
}
