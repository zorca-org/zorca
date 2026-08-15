//! The "rename workspace" modal behind the sidebar row's `…` menu.
//!
//! One field, because a rename is one fact. The name is the only thing a
//! workspace has that the user chose and can change: the id, the session, the
//! stored layout and the checkout are all identity or history, and none of them
//! moves — see [`crate::AdeWorkspace::daemon_workspace_id`].
//!
//! **The daemon is asked first.** [`WorkspaceLifecycleService::rename_workspace`]
//! writes the registry row only after the backend has accepted, so a daemon that
//! refused — an old one that does not know the frame, a host that could not be
//! reached — leaves the modal open with the reason in it rather than a name only
//! this machine would ever believe in.

use crate::{
    AdeWorkspace, AdeWorkspaceStore, WorkspaceLifecycleService, workspace_view::bound_workspace,
};
use editor::Editor;
use gpui::{DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, WeakEntity};
use std::sync::Arc;
use ui::{Divider, prelude::*};
use workspace::{ModalView, Workspace};

/// Opens the rename modal for `workspace` on `zed_workspace`, which must be the
/// window's *active* workspace or the modal lands in a layer that is not on
/// screen.
pub fn open_rename_workspace_modal(
    zed_workspace: &Entity<Workspace>,
    workspace: AdeWorkspace,
    window: &mut Window,
    cx: &mut App,
) {
    let lifecycle = crate::lifecycle_service(cx);
    let handle = zed_workspace.downgrade();
    zed_workspace.update(cx, |zed_workspace, cx| {
        zed_workspace.toggle_modal(window, cx, |window, cx| {
            RenameWorkspaceModal::new(lifecycle, handle, workspace, window, cx)
        });
    });
}

pub(crate) struct RenameWorkspaceModal {
    name_editor: Entity<Editor>,
    /// The window's active workspace, which is the one *showing* the renamed
    /// workspace only sometimes; see [`RenameWorkspaceModal::confirm`].
    zed_workspace: WeakEntity<Workspace>,
    workspace: AdeWorkspace,
    lifecycle: Arc<WorkspaceLifecycleService>,
    /// Set while the daemon is being asked, so the modal cannot be confirmed
    /// twice into two round trips.
    renaming: bool,
    error: Option<SharedString>,
}

impl RenameWorkspaceModal {
    pub(crate) fn new(
        lifecycle: Arc<WorkspaceLifecycleService>,
        zed_workspace: WeakEntity<Workspace>,
        workspace: AdeWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            // Prefilled with the current name: a rename is usually an edit of
            // what is there, not a fresh answer.
            editor.set_text(workspace.name.clone(), window, cx);
            editor
        });

        Self {
            name_editor,
            zed_workspace,
            workspace,
            lifecycle,
            renaming: false,
            error: None,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    /// Asks the daemon, then follows the new name into the window header.
    ///
    /// The sidebar's pencil opens this modal over the window's *active*
    /// workspace whatever row it was clicked on, so the header is only this
    /// rename's to change when that window is actually bound to the workspace
    /// being renamed — which [`crate::workspace_view::bound_workspace`] is the
    /// only thing that can say. A window bound to some other workspace keeps its
    /// header; a window elsewhere that is showing this one misses the update and
    /// re-asserts the name on its next open.
    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.renaming {
            return;
        }

        let name = self.name_editor.read(cx).text(cx).trim().to_owned();
        if name.is_empty() {
            self.error = Some("A workspace needs a name.".into());
            cx.notify();
            return;
        }
        if name == self.workspace.name {
            cx.emit(DismissEvent);
            return;
        }

        let lifecycle = self.lifecycle.clone();
        let id = self.workspace.id.clone();

        self.renaming = true;
        self.error = None;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            // Blocking: drives the session backend, then sqlite.
            let renamed = cx
                .background_spawn(async move { lifecycle.rename_workspace(&id, &name).await })
                .await;

            this.update_in(cx, |this, window, cx| match renamed {
                Ok(workspace) => {
                    if bound_workspace(this.zed_workspace.entity_id(), cx) == Some(&workspace.id) {
                        let name = SharedString::from(workspace.name.clone());
                        this.zed_workspace
                            .update(cx, |zed_workspace, cx| {
                                zed_workspace.set_window_title_override(Some(name), window, cx);
                            })
                            .ok();
                    }

                    this.workspace = workspace;
                    // The rows re-render off the shared store; nothing reorders,
                    // because the ledger sorts by `created_at`.
                    if let Some(store) = AdeWorkspaceStore::try_global(cx) {
                        store.update(cx, |store, cx| store.refresh(cx));
                    }
                    cx.emit(DismissEvent);
                }
                Err(error) => {
                    this.renaming = false;
                    this.error = Some(format!("{error:#}").into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}

impl EventEmitter<DismissEvent> for RenameWorkspaceModal {}
impl ModalView for RenameWorkspaceModal {}

impl Focusable for RenameWorkspaceModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name_editor.focus_handle(cx)
    }
}

impl Render for RenameWorkspaceModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("AdeRenameWorkspace")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w(rems(30.))
            .p_3()
            .gap_3()
            .child(Headline::new("Rename workspace").size(HeadlineSize::XSmall))
            .child(Divider::horizontal())
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new("Name").size(LabelSize::Small))
                            .child(
                                Label::new("shown in the sidebar; the session is unaffected")
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
                            .child(self.name_editor.clone()),
                    ),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("ade-rename-workspace-cancel", "Cancel").on_click(cx.listener(
                            |_, _, window, cx| {
                                window.dispatch_action(Box::new(menu::Cancel), cx);
                            },
                        )),
                    )
                    .child(
                        Button::new(
                            "ade-rename-workspace-confirm",
                            if self.renaming {
                                "Renaming…"
                            } else {
                                "Rename"
                            },
                        )
                        .style(ButtonStyle::Filled)
                        .disabled(self.renaming)
                        .on_click(cx.listener(|_, _, window, cx| {
                            window.dispatch_action(Box::new(menu::Confirm), cx);
                        })),
                    ),
            )
    }
}
