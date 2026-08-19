//! The "new workspace" modal behind the sidebar's `+`.
//!
//! Deliberately three fields — a human name, a repository path, and an optional
//! host. The project group is *derived* from the path's basename rather than
//! asked for, so the common case (several workspaces on one checkout) groups
//! itself, and the branch is left `None`.
//!
//! **The host decides what the path means.** Left empty, the workspace is local
//! and the path is a path here. Filled in, the path is read on *that* host and
//! is never touched locally. There is no validation past a trim: the
//! destination is whatever `ssh` accepts, and the user's `~/.ssh/config` owns
//! resolution — ADE never implements its own auth or host database.
//!
//! **The rest of the field list is deferred.** Branch and the agent command
//! both belong here eventually; they are left out so this modal stays the small
//! thing it needs to be.

use crate::{
    AdeWorkspace, WorkspaceLifecycleService, attach::attach_terminal, project_id_from_path,
};
use anyhow::Result;
use editor::Editor;
use gpui::{
    AsyncWindowContext, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, WeakEntity,
};
use std::{path::PathBuf, rc::Rc, sync::Arc};
use ui::{Divider, prelude::*};
use workspace::{ModalView, Workspace};

/// Called with the freshly created workspace, on the foreground, once
/// [`WorkspaceLifecycleService::create_workspace`] has returned.
pub type OnWorkspaceCreated = Rc<dyn Fn(AdeWorkspace, &mut Window, &mut App)>;

pub(crate) type OnCreated = OnWorkspaceCreated;

/// Opens the modal on `zed_workspace`, optionally prefilled, and hands the
/// workspace it creates to `on_created`.
///
/// `zed_workspace` must be the window's *active* workspace, or the modal lands
/// in a modal layer that is not on screen.
///
/// **`on_created` is how a caller says what "created" means to it.** The ledger
/// opens the new workspace as a whole layout — switching the window to its
/// project before attaching — which needs `MultiWorkspace` machinery this crate
/// has no business reaching into. Passing `None` takes the default: attach the
/// session into `zed_workspace` and nothing else, which is all a caller with no
/// layout of its own wants.
///
/// `remote_host` is a destination `ssh` accepts and `repository_path` is read on
/// that host; both are `None` for a workspace on this machine. Neither is
/// validated here — see the module docs.
pub fn open_create_workspace_modal(
    zed_workspace: &Entity<Workspace>,
    remote_host: Option<String>,
    repository_path: Option<String>,
    on_created: Option<OnWorkspaceCreated>,
    window: &mut Window,
    cx: &mut App,
) {
    let lifecycle = crate::lifecycle_service(cx);
    let on_created = on_created.unwrap_or_else(|| {
        let lifecycle = lifecycle.clone();
        let zed_workspace = zed_workspace.downgrade();
        Rc::new(
            move |created: AdeWorkspace, window: &mut Window, cx: &mut App| {
                attach_created(
                    zed_workspace.clone(),
                    lifecycle.clone(),
                    created,
                    window,
                    cx,
                );
            },
        )
    });

    zed_workspace.update(cx, |zed_workspace, cx| {
        zed_workspace.toggle_modal(window, cx, |window, cx| {
            CreateWorkspaceModal::new(lifecycle, on_created, window, cx).with_prefill(
                remote_host.as_deref(),
                repository_path.as_deref(),
                window,
                cx,
            )
        });
    });
}

/// Creating already made the session, so this is only the attach half — the
/// same thing selecting a sidebar row does once its session is known alive.
fn attach_created(
    zed_workspace: WeakEntity<Workspace>,
    lifecycle: Arc<WorkspaceLifecycleService>,
    created: AdeWorkspace,
    window: &mut Window,
    cx: &mut App,
) {
    window
        .spawn(cx, async move |cx| {
            let Err(error) = attach(&zed_workspace, lifecycle, created, cx).await else {
                return;
            };
            // The modal dismissed itself when creation succeeded, so the window
            // is the only place left that can say the attach failed.
            zed_workspace
                .update(cx, |zed_workspace, cx| zed_workspace.show_error(error, cx))
                .ok();
        })
        .detach();
}

async fn attach(
    zed_workspace: &WeakEntity<Workspace>,
    lifecycle: Arc<WorkspaceLifecycleService>,
    created: AdeWorkspace,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let attached = cx
        .background_spawn({
            // Blocking: the argv comes from the session backend.
            let created = created.clone();
            async move { lifecycle.attach_command(&created) }
        })
        .await?;

    // Through the shared attach rather than straight to the pane: it is what
    // hands the window's centre to the daemon and starts the layout sync, and a
    // window created this way holds daemon terminals like any other.
    attach_terminal(zed_workspace, &created, attached, cx).await
}

pub(crate) struct CreateWorkspaceModal {
    name_editor: Entity<Editor>,
    path_editor: Entity<Editor>,
    /// Empty means local. See the module docs.
    host_editor: Entity<Editor>,
    lifecycle: Arc<WorkspaceLifecycleService>,
    on_created: OnCreated,
    /// Set while the host is minting the record, so the modal cannot be
    /// confirmed twice into two workspaces.
    creating: bool,
    error: Option<SharedString>,
}

impl CreateWorkspaceModal {
    pub(crate) fn new(
        lifecycle: Arc<WorkspaceLifecycleService>,
        on_created: OnCreated,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Vector DB spike", window, cx);
            editor
        });
        let path_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("~/code/zed", window, cx);
            editor
        });
        let host_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("build-box, or user@host", window, cx);
            editor
        });

        Self {
            name_editor,
            path_editor,
            host_editor,
            lifecycle,
            on_created,
            creating: false,
            error: None,
        }
    }

    /// Seeds the two fields a caller can already answer for the user.
    ///
    /// The name is deliberately never seeded: it is the one field only the user
    /// can supply, and leaving it empty keeps the modal's own validation as the
    /// thing that stops a nameless workspace.
    pub(crate) fn with_prefill(
        self,
        remote_host: Option<&str>,
        repository_path: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        if let Some(remote_host) = remote_host {
            self.host_editor.update(cx, |editor, cx| {
                editor.set_text(remote_host, window, cx);
            });
        }
        if let Some(repository_path) = repository_path {
            self.path_editor.update(cx, |editor, cx| {
                editor.set_text(repository_path, window, cx);
            });
        }
        self
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// Moves between the fields; the modal is small enough that a cycle is the
    /// whole navigation model.
    fn select_next(&mut self, _: &menu::SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_focus(window, cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_focus(window, cx);
    }

    fn cycle_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = if self.name_editor.focus_handle(cx).is_focused(window) {
            self.path_editor.focus_handle(cx)
        } else if self.path_editor.focus_handle(cx).is_focused(window) {
            self.host_editor.focus_handle(cx)
        } else {
            self.name_editor.focus_handle(cx)
        };
        window.focus(&next, cx);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.creating {
            return;
        }

        let name = self.name_editor.read(cx).text(cx).trim().to_owned();
        let path = self.path_editor.read(cx).text(cx).trim().to_owned();
        if name.is_empty() {
            self.error = Some("A workspace needs a name.".into());
            cx.notify();
            return;
        }
        if path.is_empty() {
            self.error = Some("A workspace needs a repository path.".into());
            cx.notify();
            return;
        }

        // Empty is local. Nothing more is checked: the destination is whatever
        // ssh accepts, including an alias out of the user's config.
        let remote_host = Some(self.host_editor.read(cx).text(cx).trim().to_owned())
            .filter(|host| !host.is_empty());

        let repository_path = PathBuf::from(path);
        // The basename either way — the group is what the checkout is called,
        // and a remote path has one too. `project_id` is host-blind, so two
        // hosts with a `zed` checkout share a heading; accepted for now.
        let project_id = project_id_from_path(&repository_path);
        let lifecycle = self.lifecycle.clone();
        let on_created = self.on_created.clone();

        self.creating = true;
        self.error = None;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            // Creating drives the session backend, which blocks.
            let created = cx
                .background_spawn(async move {
                    lifecycle
                        .create_workspace(name, project_id, repository_path, None, remote_host)
                        .await
                })
                .await;

            this.update_in(cx, |this, window, cx| match created {
                Ok(workspace) => {
                    on_created(workspace, window, cx);
                    cx.emit(DismissEvent);
                }
                Err(error) => {
                    this.creating = false;
                    this.error = Some(format!("{error:#}").into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn render_field(
        &self,
        label: &'static str,
        hint: &'static str,
        editor: &Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new(label).size(LabelSize::Small))
                    .child(
                        Label::new(hint)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(cx.theme().colors().editor_background)
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(editor.clone()),
            )
    }
}

impl EventEmitter<DismissEvent> for CreateWorkspaceModal {}
impl ModalView for CreateWorkspaceModal {}

impl Focusable for CreateWorkspaceModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name_editor.focus_handle(cx)
    }
}

impl Render for CreateWorkspaceModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("AdeCreateWorkspace")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .elevation_3(cx)
            .w(rems(30.))
            .p_3()
            .gap_3()
            .child(Headline::new("New workspace").size(HeadlineSize::XSmall))
            .child(Divider::horizontal())
            .child(self.render_field("Name", "shown in the sidebar", &self.name_editor, cx))
            .child(self.render_field(
                "Repository path",
                "the project group is its folder name",
                &self.path_editor,
                cx,
            ))
            .child(self.render_field(
                "Remote host",
                "optional; empty means this machine",
                &self.host_editor,
                cx,
            ))
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("ade-create-workspace-cancel", "Cancel").on_click(cx.listener(
                            |_, _, window, cx| {
                                window.dispatch_action(Box::new(menu::Cancel), cx);
                            },
                        )),
                    )
                    .child(
                        Button::new(
                            "ade-create-workspace-confirm",
                            if self.creating {
                                "Creating…"
                            } else {
                                "Create"
                            },
                        )
                        .style(ButtonStyle::Filled)
                        .disabled(self.creating)
                        .on_click(cx.listener(|_, _, window, cx| {
                            window.dispatch_action(Box::new(menu::Confirm), cx);
                        })),
                    ),
            )
    }
}
