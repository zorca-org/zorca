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
//! - **Selecting attaches; it never repairs.** A row whose session probed
//!   [`SessionState::Dead`] does not silently get a new one. The row shows what
//!   died and offers a "Recreate" button, and only that click recreates.
//! - **Killing is reachable only from a control that says "Kill".** "Kill
//!   workspace…" sends the daemon a single `KillWorkspace`: every session in
//!   the workspace dies and the record goes with them. "Kill All Sessions…"
//!   kills the live sessions shown in this view but keeps their workspace
//!   records and layouts. "Kill and Clean Up…" freezes the displayed dead rows
//!   and re-probes them before deletion. "Stop (detach)" and "Remove from list"
//!   sit next to them and neither kills.
//! - **Every lifecycle call blocks.** The session backend is synchronous and
//!   the registry is sqlite, so nothing in this file may call the service on
//!   the foreground thread; it all goes through `cx.background_spawn`.
//!
//! **Remote workspaces are ordinary rows.** They reconcile, attach, stop and
//! kill through the backend for their host exactly as local ones do, and
//! project groups are scoped by that host. A host that cannot be reached costs
//! its own error line and leaves its rows showing their last known status —
//! never the whole tree.

use crate::{
    AdeWorkspace, AdeWorkspaceStore, SessionState, WorkspaceEntry, WorkspaceId,
    WorkspaceLifecycleService, WorkspaceStatus,
    attach::attach_terminal,
    create_workspace_modal::CreateWorkspaceModal,
    open_workspace_session,
    workspace_view::{ensure_repository_worktree, name_window_after_workspace},
};
use anyhow::{Result, bail};
use gpui::{Entity, EventEmitter, FocusHandle, Focusable, WeakEntity, actions};
use std::{collections::HashMap, future::Future, rc::Rc, sync::Arc};
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
        project_identity: String,
        remote_host: Option<String>,
        live_count: usize,
        gone_count: usize,
    },
    Workspace {
        entry: WorkspaceEntry,
    },
}

fn live_session_entries(rows: &[SidebarRow]) -> Vec<WorkspaceEntry> {
    rows.iter()
        .filter_map(|row| match row {
            SidebarRow::Workspace { entry } if entry.state() == SessionState::Alive => {
                Some(entry.clone())
            }
            _ => None,
        })
        .collect()
}

async fn kill_session_targets(
    lifecycle: Arc<WorkspaceLifecycleService>,
    targets: Vec<WorkspaceEntry>,
) -> Result<()> {
    let mut failures = Vec::new();
    for target in targets {
        let (label, result) = match target {
            WorkspaceEntry::Persisted(workspace, _) => {
                let label = workspace.id.to_string();
                let result = lifecycle
                    .kill_workspace_session(&workspace.id)
                    .await
                    .map(|_| ());
                (label, result)
            }
            WorkspaceEntry::Discovered {
                remote_host,
                workspace,
                ..
            } => {
                let wire_id = workspace.id;
                let label = wire_id.clone();
                let result = match lifecycle
                    .confirm_discovered(remote_host.as_deref(), &wire_id)
                    .await
                {
                    Ok(workspace) => lifecycle
                        .kill_workspace_session(&workspace.id)
                        .await
                        .map(|_| ()),
                    Err(error)
                        if error
                            .downcast_ref::<crate::lifecycle::WorkspaceGone>()
                            .is_some() =>
                    {
                        Ok(())
                    }
                    Err(error) => Err(error),
                };
                (label, result)
            }
        };
        if let Err(error) = result {
            failures.push(format!("{label}: {error:#}"));
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "failed to kill sessions in {} workspace{}:\n{}",
            failures.len(),
            if failures.len() == 1 { "" } else { "s" },
            failures.join("\n")
        )
    }
}

/// Flattens reconciled workspaces into project-grouped rows.
///
/// Group order is first-appearance, and within a group the input order is
/// preserved — the pass lists the rows this client used most recently first and
/// what its hosts merely hold after them, so the project you last worked in
/// floats to the top and its newest workspace leads it.
pub(crate) fn group_rows(entries: &[WorkspaceEntry]) -> Vec<SidebarRow> {
    let mut order: Vec<(Option<String>, String)> = Vec::new();
    let mut grouped: HashMap<(Option<String>, String), Vec<&WorkspaceEntry>> = HashMap::new();

    for entry in entries {
        let key = (
            entry.remote_host().map(ToOwned::to_owned),
            entry.project_identity(),
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
        let (remote_host, project_identity) = key;
        let project_id = crate::project_id_from_identity(&project_identity);
        rows.push(SidebarRow::Project {
            project_id,
            project_identity,
            remote_host,
            live_count,
            gone_count,
        });
        rows.extend(group.into_iter().map(|entry| SidebarRow::Workspace {
            entry: entry.clone(),
        }));
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
    selected: Option<WorkspaceId>,
    /// The last failed *action*, shown in the panel rather than swallowed — a
    /// session backend that will not start is exactly what the user needs to
    /// see. The store carries the failures of its own passes.
    error: Option<SharedString>,
    bulk_action_in_progress: bool,
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
            error: None,
            bulk_action_in_progress: false,
            _store_observation: store_observation,
        };
        this.take_rows_from_store(&store, cx);
        this
    }

    /// Rebuilds the tree from the store's entries. The only thing that ever
    /// writes [`Self::rows`].
    fn take_rows_from_store(&mut self, store: &Entity<AdeWorkspaceStore>, cx: &mut Context<Self>) {
        let entries = store.read(cx).entries();
        // A workspace that has been removed can no longer be the selection, or
        // the row would stay highlighted with nothing under it.
        if let Some(selected) = &self.selected
            && !entries
                .iter()
                .filter_map(WorkspaceEntry::persisted)
                .any(|(workspace, _)| &workspace.id == selected)
        {
            self.selected = None;
        }
        self.rows = group_rows(entries);
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

    /// Clicking a row: hand it to the shared open path, which marks the
    /// workspace opened, probes its session, and attaches unless it is dead —
    /// see [`open_workspace_session`].
    fn select_workspace(&mut self, id: WorkspaceId, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id.clone());
        cx.notify();

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

    /// Clicking a discovered row: confirm it first, which is what records that
    /// this client uses the workspace, then open the row that comes back
    /// exactly as any other row is opened.
    ///
    /// A record that vanished between the listing and the click comes back as
    /// [`crate::lifecycle::WorkspaceGone`]; the reconcile that follows drops it from the
    /// tree, so nothing is persisted for a workspace that no longer exists.
    fn open_discovered(
        &mut self,
        remote_host: Option<String>,
        wire_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let lifecycle = self.lifecycle.clone();
        cx.spawn_in(window, async move |this, cx| {
            let confirmed = cx
                .background_spawn(async move {
                    lifecycle
                        .confirm_discovered(remote_host.as_deref(), &wire_id)
                        .await
                })
                .await;
            match confirmed {
                Ok(workspace) => {
                    this.update_in(cx, |this, window, cx| {
                        this.select_workspace(workspace.id, window, cx);
                        this.reconcile(cx);
                    })
                    .ok();
                }
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.error = error
                            .downcast_ref::<crate::lifecycle::WorkspaceGone>()
                            .is_none()
                            .then(|| format!("{error:#}").into());
                        this.reconcile(cx);
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    /// The explicit repair for a dead session, from the button the dead row
    /// renders. The connect flow (`crate::connect`) runs the same repair when
    /// a fresh connection lands on a workspace with nothing alive — those are
    /// the only two paths that reach
    /// [`WorkspaceLifecycleService::recreate_session`].
    fn recreate_and_attach(
        &mut self,
        id: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some(id.clone());
        cx.notify();

        let lifecycle = self.lifecycle.clone();
        let zed_workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let recreated = cx
                .background_spawn(async move {
                    let workspace = lifecycle.recreate_session(&id).await?;
                    let attached = lifecycle.attach_command(&workspace)?;
                    anyhow::Ok((workspace, attached))
                })
                .await;

            let outcome = match recreated {
                Ok((workspace, attached)) => {
                    // Same as selecting: the window follows the workspace
                    // before the terminal lands.
                    ensure_repository_worktree(&zed_workspace, &workspace, cx).await;
                    match name_window_after_workspace(&zed_workspace, &workspace, cx) {
                        Ok(()) => attach_terminal(&zed_workspace, &workspace, attached, cx).await,
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(error),
            };

            this.update(cx, |this, cx| {
                this.error = outcome.err().map(|error| format!("{error:#}").into());
                this.reconcile(cx);
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

    /// The row-level killing path in this panel takes the **whole
    /// workspace**: every session in it dies and the daemon's record of it —
    /// its layout included — is deleted, which is what
    /// [`WorkspaceLifecycleService::kill_workspace`] sends as a single
    /// `KillWorkspace`. Other clients showing it are told and stop syncing.
    ///
    /// Destructive and unconfirmed: the menu entry's own label ("Kill
    /// workspace…") is the guard. Do not reach this from any control that does not say
    /// "Kill", and do not soften the label — "Kill session" would understate
    /// what goes.
    fn kill_workspace(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        self.run_action(cx, move |lifecycle| async move {
            lifecycle.kill_workspace(&id).await.map(|_| ())
        });
    }

    /// Forgets the workspace.
    ///
    /// **Registry only — this must never touch the session backend.** A
    /// removed workspace's session goes on running and can be discovered again
    /// later; that is exactly what makes removal safe to offer without a
    /// confirmation, and exactly what would be destroyed by "helpfully"
    /// killing the session here.
    fn remove_workspace(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        if self.selected.as_ref() == Some(&id) {
            self.selected = None;
        }
        self.run_action(cx, move |lifecycle| async move {
            lifecycle.registry().delete_workspace(id).await
        });
    }

    fn kill_all_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let targets = live_session_entries(&self.rows);
        let live_count = targets.len();
        if live_count == 0 || self.bulk_action_in_progress {
            return;
        }

        let suffix = if live_count == 1 { "" } else { "s" };
        let confirmation = window.prompt(
            gpui::PromptLevel::Critical,
            &format!("Kill sessions in {live_count} live workspace{suffix}?"),
            Some(
                "This terminates the sessions active when each shown workspace is processed, including their agents and child processes. Workspace records, saved layouts, repository files, and host daemons are kept.",
            ),
            &["Kill All Sessions", "Cancel"],
            cx,
        );
        let lifecycle = self.lifecycle.clone();

        cx.spawn(async move |this, cx| {
            if confirmation.await != Ok(0) {
                return;
            }
            let started = this
                .update(cx, |this, cx| {
                    if this.bulk_action_in_progress {
                        return false;
                    }
                    this.bulk_action_in_progress = true;
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !started {
                return;
            }

            let result = cx
                .background_spawn(async move { kill_session_targets(lifecycle, targets).await })
                .await;
            this.update(cx, |this, cx| {
                this.bulk_action_in_progress = false;
                this.error = result.err().map(|error| format!("{error:#}").into());
                this.reconcile(cx);
            })
            .ok();
        })
        .detach();
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
                                // Creating already made the session; selecting
                                // it is what attaches a terminal to it.
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

    /// Rows only: a discovery is nothing this client has used, so cleanup —
    /// which kills daemon records — must not reach one.
    fn gone_workspace_ids(&self) -> Vec<WorkspaceId> {
        self.rows
            .iter()
            .filter_map(|row| match row {
                SidebarRow::Workspace { entry } => entry.persisted(),
                _ => None,
            })
            .filter(|(_, state)| *state == SessionState::Dead)
            .map(|(workspace, _)| workspace.id.clone())
            .collect()
    }

    fn cleanup_gone_workspaces(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let candidate_ids = self.gone_workspace_ids();
        let gone_count = candidate_ids.len();
        if gone_count == 0 || self.bulk_action_in_progress {
            return;
        }

        let suffix = if gone_count == 1 { "" } else { "s" };
        let confirmation = window.prompt(
            gpui::PromptLevel::Critical,
            &format!("Kill and clean up {gone_count} gone workspace{suffix}?"),
            Some(
                "This permanently deletes their daemon workspace records and saved layouts. If a session starts in one before cleanup finishes, that session is terminated. Other live sessions and repository files are not changed.",
            ),
            &["Kill and Clean Up", "Cancel"],
            cx,
        );
        let lifecycle = self.lifecycle.clone();

        cx.spawn(async move |this, cx| {
            if confirmation.await != Ok(0) {
                return;
            }
            let started = this
                .update(cx, |this, cx| {
                    if this.bulk_action_in_progress {
                        return false;
                    }
                    this.bulk_action_in_progress = true;
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !started {
                return;
            }

            let result = cx
                .background_spawn(
                    async move { lifecycle.cleanup_dead_workspaces(candidate_ids).await },
                )
                .await;
            this.update(cx, |this, cx| {
                this.bulk_action_in_progress = false;
                this.error = result.err().map(|error| format!("{error:#}").into());
                this.reconcile(cx);
            })
            .ok();
        })
        .detach();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let gone_count = self.gone_workspace_ids().len();
        let live_count = live_session_entries(&self.rows).len();
        h_flex()
            .h(rems(1.75))
            .px_2()
            .justify_between()
            .child(Label::new("Workspaces"))
            .child(
                h_flex()
                    .gap_1()
                    .when(live_count > 0, |this| {
                        this.child(
                            Button::new("ade-kill-all-sessions", "Kill All Sessions…")
                                .label_size(LabelSize::Small)
                                .disabled(self.bulk_action_in_progress)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.kill_all_sessions(window, cx);
                                })),
                        )
                    })
                    .when(gone_count > 0, |this| {
                        this.child(
                            Button::new(
                                "ade-cleanup-gone-workspaces",
                                "Kill and Clean Up Gone Workspaces…",
                            )
                            .label_size(LabelSize::Small)
                            .disabled(self.bulk_action_in_progress)
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.cleanup_gone_workspaces(window, cx);
                                },
                            )),
                        )
                    })
                    .child(
                        IconButton::new("ade-new-workspace", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .aria_label("New workspace")
                            .tooltip(Tooltip::text("New workspace"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.create_workspace(window, cx);
                            })),
                    ),
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
        match entry {
            WorkspaceEntry::Persisted(workspace, state) => {
                self.render_persisted_row(ix, workspace, *state, cx)
            }
            WorkspaceEntry::Discovered {
                remote_host,
                workspace,
                state,
            } => self.render_discovered_row(ix, entry, remote_host, &workspace.id, *state, cx),
        }
    }

    /// A workspace a host's daemon holds and this client has never opened.
    ///
    /// **No destructive controls**, and no rename or recreate: every one of
    /// them addresses a registry row, and there is none until the click below
    /// confirms one. Promotion is what earns a row its context menu.
    fn render_discovered_row(
        &self,
        ix: usize,
        entry: &WorkspaceEntry,
        remote_host: &Option<String>,
        wire_id: &str,
        state: SessionState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let dot_color = status_color(match state {
            SessionState::Alive => WorkspaceStatus::Running,
            SessionState::Dead => WorkspaceStatus::Disconnected,
            SessionState::NeverCreated | SessionState::Unknown => WorkspaceStatus::Stopped,
        });
        let name = entry.name();
        let repository_path = entry.repository_path().to_string_lossy().into_owned();
        let wire_id = wire_id.to_owned();
        let remote_host = remote_host.clone();

        ListItem::new(("ade-discovered-item", ix))
            .spacing(ListItemSpacing::Sparse)
            .indent_level(1)
            .start_slot(Indicator::dot().color(dot_color))
            .child(
                v_flex()
                    .w_full()
                    .min_w_0()
                    .child(Label::new(name).color(Color::Muted).truncate())
                    .child(
                        Label::new(format!("{wire_id} · {repository_path}"))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .buffer_font(cx)
                            .truncate(),
                    ),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_discovered(remote_host.clone(), wire_id.clone(), window, cx);
            }))
            .into_any_element()
    }

    fn render_persisted_row(
        &self,
        ix: usize,
        workspace: &AdeWorkspace,
        state: SessionState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = workspace.id.clone();
        let is_selected = self.selected.as_ref() == Some(&id);
        let branch = workspace.branch.clone();
        let name = workspace.name.clone();
        let dot_color = status_color(workspace_status(workspace, state));
        let repository_path = workspace.repository_path.to_string_lossy().into_owned();
        let daemon_workspace_id = workspace.daemon_workspace_id();
        // The row's element trees outlive this call, so nothing borrowed from
        // `workspace` or `cx` may be captured — take owned copies first.
        let this = cx.entity().downgrade();

        let row = {
            let this_for_click = this.clone();
            let this_for_menu = this;
            let id_for_click = id.clone();
            let id_for_menu = id.clone();
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
                                    Label::new(format!(
                                        "{daemon_workspace_id} · {repository_path}"
                                    ))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .buffer_font(cx)
                                    .truncate(),
                                ),
                        )
                        .on_click(move |_, window, cx| {
                            this_for_click
                                .update(cx, |this, cx| {
                                    this.select_workspace(id_for_click.clone(), window, cx)
                                })
                                .ok();
                        })
                })
                .menu(move |window, cx| {
                    let this = this_for_menu.clone();
                    let id = id_for_menu.clone();
                    ContextMenu::build(window, cx, move |menu, _, _| {
                        menu.entry("Reconnect", None, {
                            let this = this.clone();
                            let id = id.clone();
                            move |window, cx| {
                                this.update(cx, |this, cx| {
                                    this.select_workspace(id.clone(), window, cx)
                                })
                                .ok();
                            }
                        })
                        // Detaches. The session and everything in it survives.
                        .entry("Stop (detach)", None, {
                            let this = this.clone();
                            let id = id.clone();
                            move |_, cx| {
                                this.update(cx, |this, cx| this.stop_workspace(id.clone(), cx))
                                    .ok();
                            }
                        })
                        .separator()
                        // The only entry that kills, and it says so — and says
                        // what: every session in the workspace, and the
                        // workspace record with them.
                        .entry("Kill workspace…", None, {
                            let this = this.clone();
                            let id = id.clone();
                            move |_, cx| {
                                this.update(cx, |this, cx| this.kill_workspace(id.clone(), cx))
                                    .ok();
                            }
                        })
                        // Registry only; never touches the session backend.
                        .entry("Remove from list", None, {
                            move |_, cx| {
                                this.update(cx, |this, cx| this.remove_workspace(id.clone(), cx))
                                    .ok();
                            }
                        })
                    })
                })
        };

        if state != SessionState::Dead {
            return row.into_any_element();
        }

        // Dead: say what died and offer the repair, but do not perform it.
        let session = workspace
            .terminal_session_id
            .clone()
            .unwrap_or_else(|| workspace.tmux_session_name());
        v_flex()
            .w_full()
            .child(row)
            .child(
                h_flex()
                    .pl_6()
                    .pr_2()
                    .pb_1()
                    .gap_1()
                    .child(
                        Label::new(format!("Session {session} is gone"))
                            .size(LabelSize::Small)
                            .color(Color::Warning)
                            .truncate(),
                    )
                    .child(
                        Button::new(("ade-recreate-session", ix), "Recreate")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.recreate_and_attach(id.clone(), window, cx);
                            })),
                    ),
            )
            .into_any_element()
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

impl Render for WorkspaceSidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows.clone();
        let selected_group = self.rows.iter().find_map(|row| match row {
            SidebarRow::Workspace { entry } => {
                let (workspace, _) = entry.persisted()?;
                (self.selected.as_ref() == Some(&workspace.id))
                    .then(|| (workspace.remote_host.clone(), workspace.project_identity()))
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
                                project_identity,
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
                                            && selected_project == project_identity
                                    },
                                ),
                                cx,
                            ),
                            SidebarRow::Workspace { entry } => {
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
        AdeWorkspaceRegistry, Attached, BackendWorkspace, SessionBackend, SessionId, SessionInfo,
        SessionSpec, StatusDelivery,
    };
    use gpui::{Subscription, TestAppContext};
    use std::{
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    #[derive(Default)]
    struct PromptBackend {
        calls: AtomicUsize,
        workspaces: Vec<BackendWorkspace>,
        session_exists: bool,
        killed: Mutex<Vec<String>>,
    }

    impl PromptBackend {
        fn new() -> Self {
            Self::default()
        }

        fn with_live_workspaces(workspaces: Vec<BackendWorkspace>) -> Self {
            Self {
                workspaces,
                session_exists: true,
                ..Self::default()
            }
        }

        fn record_call(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn killed(&self) -> Vec<String> {
            self.killed
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }
    }

    impl SessionBackend for PromptBackend {
        fn create(&self, _: &SessionSpec, _: Option<&str>) -> Result<SessionId> {
            self.record_call();
            anyhow::bail!("unexpected create")
        }

        fn list(&self) -> Result<Vec<SessionInfo>> {
            self.record_call();
            Ok(Vec::new())
        }

        fn exists(&self, _: &SessionId, _: Option<&str>) -> Result<bool> {
            self.record_call();
            Ok(self.session_exists)
        }

        fn attach(&self, _: &SessionSpec, _: Option<&str>) -> Result<Attached> {
            self.record_call();
            anyhow::bail!("unexpected attach")
        }

        fn detach(&self, _: &SessionId) -> Result<()> {
            self.record_call();
            Ok(())
        }

        fn kill(&self, id: &SessionId, _: Option<&str>) -> Result<()> {
            self.record_call();
            self.killed
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(id.to_string());
            Ok(())
        }

        fn status_delivery(&self) -> StatusDelivery {
            StatusDelivery::Push
        }

        fn instance_id(&self) -> Option<String> {
            self.session_exists.then(|| "daemon-a".to_owned())
        }

        fn list_workspaces(&self) -> Result<Vec<BackendWorkspace>> {
            Ok(self.workspaces.clone())
        }
    }

    fn entry(
        name: &str,
        project_id: &str,
        status: WorkspaceStatus,
        state: SessionState,
    ) -> WorkspaceEntry {
        let mut workspace = AdeWorkspace::new(
            name,
            project_id,
            PathBuf::from(format!("/repos/{project_id}")),
        );
        workspace.status = status;
        WorkspaceEntry::Persisted(workspace, state)
    }

    fn with_project_identity(entry: WorkspaceEntry, identity: &str) -> WorkspaceEntry {
        match entry {
            WorkspaceEntry::Persisted(mut workspace, state) => {
                workspace.project_identity = Some(identity.to_owned());
                WorkspaceEntry::Persisted(workspace, state)
            }
            entry => entry,
        }
    }

    /// The same row, on a named host.
    fn entry_on(
        host: &str,
        name: &str,
        project_id: &str,
        status: WorkspaceStatus,
        state: SessionState,
    ) -> WorkspaceEntry {
        let WorkspaceEntry::Persisted(mut workspace, state) =
            entry(name, project_id, status, state)
        else {
            unreachable!("entry builds a row")
        };
        workspace.remote_host = Some(host.to_owned());
        WorkspaceEntry::Persisted(workspace, state)
    }

    /// A workspace a host holds that this client has never opened.
    fn discovered(name: &str, root: &str, state: SessionState) -> WorkspaceEntry {
        WorkspaceEntry::Discovered {
            remote_host: None,
            workspace: crate::BackendWorkspace {
                id: name.to_owned(),
                name: name.to_owned(),
                project_id: None,
                project_identity: None,
                project_root: root.to_owned(),
                project_scope_rev: 0,
                created_at: 1_700_000_000,
            },
            state,
        }
    }

    #[test]
    fn test_group_rows_preserves_order_and_counts() {
        let entries = vec![
            with_project_identity(
                entry(
                    "spike",
                    "seedance2-5",
                    WorkspaceStatus::Running,
                    SessionState::Alive,
                ),
                "/repos/viral-studio",
            ),
            entry(
                "elsewhere",
                "praxis",
                WorkspaceStatus::Stopped,
                SessionState::NeverCreated,
            ),
            // The canonical project checkout has a different leaf label but
            // the same identity, so it rejoins the first group without making
            // that first linked checkout's label the heading.
            with_project_identity(
                entry(
                    "main",
                    "viral-studio",
                    WorkspaceStatus::Disconnected,
                    SessionState::Dead,
                ),
                "/repos/viral-studio",
            ),
        ];

        let rows = group_rows(&entries);
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[0],
            SidebarRow::Project {
                project_id: "viral-studio".into(),
                project_identity: "/repos/viral-studio".into(),
                remote_host: None,
                live_count: 1,
                gone_count: 1,
            }
        );
        assert_eq!(
            rows[1],
            SidebarRow::Workspace {
                entry: entries[0].clone(),
            }
        );
        assert_eq!(
            rows[2],
            SidebarRow::Workspace {
                entry: entries[2].clone(),
            }
        );
        // The second project keeps first-appearance order, so it follows.
        assert_eq!(
            rows[3],
            SidebarRow::Project {
                project_id: "praxis".into(),
                project_identity: "/repos/praxis".into(),
                remote_host: None,
                live_count: 0,
                gone_count: 0,
            }
        );
        assert_eq!(
            rows[4],
            SidebarRow::Workspace {
                entry: entries[1].clone(),
            }
        );
    }

    /// A discovery groups by the project its root names, so it lands beside the
    /// rows of the same checkout rather than under a heading of its own.
    #[test]
    fn test_group_rows_place_discoveries_in_their_project() {
        let entries = vec![
            entry("main", "zed", WorkspaceStatus::Running, SessionState::Alive),
            discovered("ade-zed-2de8b3", "/repos/zed", SessionState::Alive),
            discovered("ade-praxis-0f1e2d", "/repos/praxis", SessionState::Dead),
        ];

        let rows = group_rows(&entries);
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[0],
            SidebarRow::Project {
                project_id: "zed".into(),
                project_identity: "/repos/zed".into(),
                remote_host: None,
                live_count: 2,
                gone_count: 0,
            }
        );
        assert_eq!(
            rows[2],
            SidebarRow::Workspace {
                entry: entries[1].clone(),
            }
        );
        assert_eq!(
            rows[3],
            SidebarRow::Project {
                project_id: "praxis".into(),
                project_identity: "/repos/praxis".into(),
                remote_host: None,
                live_count: 0,
                gone_count: 1,
            }
        );
    }

    #[test]
    fn test_group_rows_of_nothing_is_nothing() {
        assert!(group_rows(&[]).is_empty());
    }

    #[test]
    fn test_group_rows_scopes_projects_by_host() {
        let entries = vec![
            entry_on(
                "user@host-a",
                "first",
                "viral-studio",
                WorkspaceStatus::Running,
                SessionState::Alive,
            ),
            entry_on(
                "user@host-b",
                "other-host",
                "viral-studio",
                WorkspaceStatus::Running,
                SessionState::Alive,
            ),
            entry_on(
                "user@host-a",
                "second",
                "viral-studio",
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
                project_identity: "/repos/viral-studio".into(),
                remote_host: Some("user@host-a".into()),
                live_count: 1,
                gone_count: 1,
            }
        );
        assert_eq!(
            rows[3],
            SidebarRow::Project {
                project_id: "viral-studio".into(),
                project_identity: "/repos/viral-studio".into(),
                remote_host: Some("user@host-b".into()),
                live_count: 1,
                gone_count: 0,
            }
        );
    }

    #[test]
    fn test_group_rows_separates_same_label_with_different_project_identities() {
        let entries = vec![
            with_project_identity(
                entry_on(
                    "user@host",
                    "first",
                    "viral-studio",
                    WorkspaceStatus::Running,
                    SessionState::Alive,
                ),
                "/repos/one/viral-studio",
            ),
            with_project_identity(
                entry_on(
                    "user@host",
                    "second",
                    "viral-studio",
                    WorkspaceStatus::Running,
                    SessionState::Alive,
                ),
                "/repos/two/viral-studio",
            ),
        ];

        let rows = group_rows(&entries);
        let projects = rows
            .iter()
            .filter_map(|row| match row {
                SidebarRow::Project {
                    project_id,
                    project_identity,
                    ..
                } => Some((project_id.as_str(), project_identity.as_str())),
                SidebarRow::Workspace { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            projects,
            vec![
                ("viral-studio", "/repos/one/viral-studio"),
                ("viral-studio", "/repos/two/viral-studio"),
            ]
        );
    }

    #[test]
    fn test_kill_all_targets_only_live_visible_sessions() {
        let entries = vec![
            entry("main", "zed", WorkspaceStatus::Running, SessionState::Alive),
            entry(
                "gone",
                "zed",
                WorkspaceStatus::Disconnected,
                SessionState::Dead,
            ),
            discovered("ade-praxis-live", "/repos/praxis", SessionState::Alive),
            discovered("ade-praxis-gone", "/repos/praxis", SessionState::Dead),
        ];

        let targets = live_session_entries(&group_rows(&entries));
        assert!(
            matches!(
                targets.as_slice(),
                [
                    WorkspaceEntry::Persisted(_, _),
                    WorkspaceEntry::Discovered { workspace, .. }
                ] if workspace.id == "ade-praxis-live"
            ),
            "only the live persisted and discovered rows must be in kill-all scope: {targets:?}"
        );
    }

    #[gpui::test]
    async fn test_kill_all_keeps_persisted_and_discovered_workspace_records() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_kill_all_session_targets").await;
        let mut persisted = AdeWorkspace::new("main", "zed", PathBuf::from("/repos/zed"));
        persisted.terminal_session_id = Some("ade-zed-live".to_owned());
        persisted.daemon_id = Some("daemon-a".to_owned());
        registry
            .create_workspace(persisted.clone())
            .await
            .expect("the persisted workspace should be registered");

        let discovered_workspace = BackendWorkspace {
            id: "ade-praxis-live".to_owned(),
            name: "praxis".to_owned(),
            project_id: Some("praxis".to_owned()),
            project_identity: Some("/repos/praxis".to_owned()),
            project_root: "/repos/praxis".to_owned(),
            project_scope_rev: 0,
            created_at: 1_700_000_000,
        };
        let backend = Arc::new(PromptBackend::with_live_workspaces(vec![
            discovered_workspace.clone(),
        ]));
        let lifecycle = Arc::new(WorkspaceLifecycleService::with_backend(
            registry,
            backend.clone(),
        ));

        kill_session_targets(
            lifecycle.clone(),
            vec![
                WorkspaceEntry::Persisted(persisted, SessionState::Alive),
                WorkspaceEntry::Discovered {
                    remote_host: None,
                    workspace: discovered_workspace,
                    state: SessionState::Alive,
                },
            ],
        )
        .await
        .expect("all visible sessions should be killed");

        assert_eq!(backend.killed(), vec!["ade-zed-live", "ade-praxis-live"]);
        let workspaces = lifecycle
            .registry()
            .list_workspaces()
            .expect("workspace records should remain readable");
        assert_eq!(workspaces.len(), 2);
        let mut mappings = workspaces
            .iter()
            .map(|workspace| {
                (
                    workspace.terminal_session_id.clone(),
                    workspace.daemon_id.clone(),
                )
            })
            .collect::<Vec<_>>();
        mappings.sort();
        assert_eq!(
            mappings,
            vec![
                (
                    Some("ade-praxis-live".to_owned()),
                    Some("daemon-a".to_owned())
                ),
                (Some("ade-zed-live".to_owned()), Some("daemon-a".to_owned())),
            ],
            "kill-all must retain each daemon workspace mapping"
        );
    }

    #[gpui::test]
    async fn test_kill_all_accepts_a_discovery_removed_by_another_client() {
        let registry = AdeWorkspaceRegistry::open_test_db("test_kill_all_gone_discovery").await;
        let lifecycle = Arc::new(WorkspaceLifecycleService::with_backend(
            registry,
            Arc::new(PromptBackend::with_live_workspaces(Vec::new())),
        ));

        kill_session_targets(
            lifecycle,
            vec![discovered(
                "ade-already-gone",
                "/repos/viral-studio",
                SessionState::Alive,
            )],
        )
        .await
        .expect("another client already achieved the requested result");
    }

    #[gpui::test]
    async fn test_store_reports_its_initial_refresh(cx: &mut TestAppContext) {
        let registry = AdeWorkspaceRegistry::open_test_db("test_store_initial_refresh").await;
        let lifecycle = Arc::new(WorkspaceLifecycleService::with_backend(
            registry,
            Arc::new(PromptBackend::new()),
        ));
        cx.update(|cx| cx.set_global(crate::GlobalLifecycleService(lifecycle)));

        let store = cx.update(AdeWorkspaceStore::global);
        assert!(store.read_with(cx, |store, _| store.is_refreshing()));
        cx.run_until_parked();
        assert!(!store.read_with(cx, |store, _| store.is_refreshing()));
    }

    #[gpui::test]
    async fn test_cleanup_prompt_cancel_does_not_run_and_can_retry(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let registry = AdeWorkspaceRegistry::open_test_db("test_cleanup_prompt_cancel").await;
        let mut dead_workspace = AdeWorkspace::new("gone", "zed", PathBuf::from("/repos/zed"));
        dead_workspace.status = WorkspaceStatus::Disconnected;
        dead_workspace.terminal_session_id = Some(dead_workspace.daemon_workspace_id());
        registry
            .create_workspace(dead_workspace.clone())
            .await
            .expect("dead test workspace should be registered");

        let backend = Arc::new(PromptBackend::new());
        let lifecycle = Arc::new(WorkspaceLifecycleService::with_backend(
            registry,
            backend.clone(),
        ));
        cx.update(|cx| cx.set_global(crate::GlobalLifecycleService(lifecycle.clone())));

        let rows = group_rows(&[WorkspaceEntry::Persisted(
            dead_workspace,
            SessionState::Dead,
        )]);
        let (sidebar, cx) = cx.add_window_view(|_, cx| WorkspaceSidebar {
            workspace: WeakEntity::new_invalid(),
            lifecycle,
            store: AdeWorkspaceStore::global(cx),
            focus_handle: cx.focus_handle(),
            rows,
            selected: None,
            error: None,
            bulk_action_in_progress: false,
            _store_observation: Subscription::new(|| {}),
        });
        cx.run_until_parked();
        let calls_before_prompt = backend.call_count();

        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.cleanup_gone_workspaces(window, cx);
        });
        let (title, detail) = cx.pending_prompt().expect("cleanup confirmation prompt");
        assert_eq!(title, "Kill and clean up 1 gone workspace?");
        assert!(
            detail.contains("If a session starts in one before cleanup finishes"),
            "the prompt must disclose the final probe-to-kill race: {detail}"
        );
        assert!(
            detail.contains("Other live sessions and repository files are not changed"),
            "the prompt must identify what remains outside cleanup scope: {detail}"
        );

        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(backend.call_count(), calls_before_prompt);
        assert!(!cx.has_pending_prompt());

        sidebar.update_in(cx, |sidebar, window, cx| {
            sidebar.cleanup_gone_workspaces(window, cx);
        });
        assert!(
            cx.has_pending_prompt(),
            "cancelling must leave cleanup available for another attempt"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(backend.call_count(), calls_before_prompt);
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
        let WorkspaceEntry::Persisted(mut workspace, _) =
            entry("main", "zed", WorkspaceStatus::Running, SessionState::Alive)
        else {
            unreachable!("entry builds a row")
        };
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
}
