use gpui::{ClickEvent, DismissEvent, EventEmitter, FocusHandle, Focusable, Render, WeakEntity};
use project::project_settings::ProjectSettings;
use remote::RemoteConnectionOptions;
use settings::Settings;
use ui::{ElevationIndex, Modal, ModalFooter, ModalHeader, Section, prelude::*};
use workspace::{
    ModalView, MultiWorkspace, OpenOptions, Workspace, notifications::DetachAndPromptErr,
};

use crate::open_remote_project;

enum Host {
    CollabGuestProject,
    RemoteServerProject(RemoteConnectionOptions, bool),
}

pub struct DisconnectedOverlay {
    workspace: WeakEntity<Workspace>,
    host: Host,
    focus_handle: FocusHandle,
    finished: bool,
}

impl EventEmitter<DismissEvent> for DisconnectedOverlay {}
impl Focusable for DisconnectedOverlay {
    fn focus_handle(&self, _cx: &gpui::App) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}
impl ModalView for DisconnectedOverlay {
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> workspace::DismissDecision {
        workspace::DismissDecision::Dismiss(self.finished)
    }
    fn fade_out_background(&self) -> bool {
        true
    }
}

impl DisconnectedOverlay {
    pub fn register(
        workspace: &mut Workspace,
        window: Option<&mut Window>,
        cx: &mut Context<Workspace>,
    ) {
        let Some(window) = window else {
            return;
        };
        cx.subscribe_in(
            workspace.project(),
            window,
            |workspace, project, event, window, cx| {
                if !matches!(
                    event,
                    project::Event::DisconnectedFromHost
                        | project::Event::DisconnectedFromRemote { .. }
                ) {
                    return;
                }
                let handle = cx.entity().downgrade();

                let remote_connection_options = project.read(cx).remote_connection_options(cx);
                let host = if let Some(remote_connection_options) = remote_connection_options {
                    Host::RemoteServerProject(
                        remote_connection_options,
                        matches!(
                            event,
                            project::Event::DisconnectedFromRemote {
                                server_not_running: true
                            }
                        ),
                    )
                } else {
                    Host::CollabGuestProject
                };

                workspace.toggle_modal(window, cx, |_, cx| DisconnectedOverlay {
                    finished: false,
                    workspace: handle,
                    host,
                    focus_handle: cx.focus_handle(),
                });
            },
        )
        .detach();
    }

    fn handle_reconnect(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss(cx);

        if let Host::RemoteServerProject(remote_connection_options, _) = &self.host {
            self.reconnect_to_remote_project(remote_connection_options.clone(), window, cx);
        }
    }

    fn reconnect_to_remote_project(
        &self,
        connection_options: RemoteConnectionOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        let Some(window_handle) = window.window_handle().downcast::<MultiWorkspace>() else {
            return;
        };

        let app_state = workspace.read(cx).app_state().clone();
        let paths = workspace
            .read(cx)
            .root_paths(cx)
            .iter()
            .map(|path| path.to_path_buf())
            .collect();

        cx.spawn_in(window, async move |_, cx| {
            open_remote_project(
                connection_options,
                paths,
                app_state,
                OpenOptions {
                    requesting_window: Some(window_handle),
                    ..Default::default()
                },
                cx,
            )
            .await?;
            Ok(())
        })
        .detach_and_prompt_err("Failed to reconnect", window, cx, |_, _, _| None);
    }

    fn handle_close_workspace(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(multi_workspace) = workspace
            .read(cx)
            .multi_workspace()
            .and_then(WeakEntity::upgrade)
        else {
            return;
        };

        self.dismiss(cx);
        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace
                .close_workspace(&workspace, window, cx)
                .detach_and_log_err(cx);
        });
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.finished = true;
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.dismiss(cx);
    }
}

impl Render for DisconnectedOverlay {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_reconnect = matches!(self.host, Host::RemoteServerProject(..));

        let message = match &self.host {
            Host::CollabGuestProject => {
                "Your connection to the remote project has been lost.".to_string()
            }
            Host::RemoteServerProject(options, server_not_running) => {
                let autosave = if ProjectSettings::get_global(cx)
                    .session
                    .restore_unsaved_buffers
                {
                    "\nUnsaved changes are stored locally."
                } else {
                    ""
                };
                let reason = if *server_not_running {
                    "process exiting unexpectedly"
                } else {
                    "not responding"
                };
                format!(
                    "Your connection to {} has been lost due to the server {reason}.{autosave}",
                    options.display_name(),
                )
            }
        };

        div()
            .track_focus(&self.focus_handle(cx))
            .elevation_3(cx)
            .on_action(cx.listener(Self::cancel))
            .occlude()
            .w(rems(24.))
            .max_h(rems(40.))
            .child(
                Modal::new("disconnected", None)
                    .header(
                        ModalHeader::new()
                            .show_dismiss_button(true)
                            .on_dismiss(cx.listener(|this, _, _, cx| this.dismiss(cx)))
                            .child(Headline::new("Disconnected").size(HeadlineSize::Small)),
                    )
                    .section(Section::new().child(Label::new(message)))
                    .footer(
                        ModalFooter::new().end_slot(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .debug_selector(|| {
                                            "disconnected-close-workspace".to_owned()
                                        })
                                        .child(
                                            Button::new("close-workspace", "Close Workspace")
                                                .style(ButtonStyle::Filled)
                                                .layer(ElevationIndex::ModalSurface)
                                                .on_click(
                                                    cx.listener(Self::handle_close_workspace),
                                                ),
                                        ),
                                )
                                .when(can_reconnect, |el| {
                                    el.child(
                                        Button::new("reconnect", "Reconnect")
                                            .style(ButtonStyle::Filled)
                                            .layer(ElevationIndex::ModalSurface)
                                            .start_icon(Icon::new(IconName::ArrowCircle))
                                            .on_click(cx.listener(Self::handle_reconnect)),
                                    )
                                }),
                        ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::{Entity, Modifiers, TestAppContext};
    use project::Project;
    use workspace::AppState;

    use super::*;

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            let app_state = AppState::test(cx);
            crate::init(cx);
            editor::init(cx);
            app_state
        })
    }

    fn show_disconnected_overlay(workspace: &Entity<Workspace>, cx: &mut gpui::VisualTestContext) {
        let weak_workspace = workspace.downgrade();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, |_, cx| DisconnectedOverlay {
                workspace: weak_workspace,
                host: Host::CollabGuestProject,
                focus_handle: cx.focus_handle(),
                finished: false,
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
    }

    #[gpui::test]
    async fn close_icon_dismisses_disconnected_overlay(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        show_disconnected_overlay(&workspace, cx);
        let workspace_focus = workspace.read_with(cx, |workspace, cx| workspace.focus_handle(cx));
        cx.update(|window, cx| workspace_focus.focus(window, cx));

        let close_bounds = cx
            .debug_bounds("ICON-Close")
            .expect("disconnected overlay close icon should render");
        cx.simulate_click(close_bounds.center(), Modifiers::none());

        assert!(workspace.read_with(cx, |workspace, cx| {
            workspace.active_modal::<DisconnectedOverlay>(cx).is_none()
        }));
    }

    #[gpui::test]
    async fn close_disconnected_workspace_keeps_window_open(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let replacement_project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        multi_workspace.update_in(cx, |multi_workspace, window, cx| {
            multi_workspace.test_add_workspace(replacement_project, window, cx);
            multi_workspace.activate(workspace.clone(), None, window, cx);
        });
        show_disconnected_overlay(&workspace, cx);

        let close_bounds = cx
            .debug_bounds("disconnected-close-workspace")
            .expect("close workspace button should render");
        cx.simulate_click(close_bounds.center(), Modifiers::none());
        cx.run_until_parked();

        assert_eq!(cx.update(|_, cx| cx.windows().len()), 1);
        multi_workspace.read_with(cx, |multi_workspace, _| {
            assert_ne!(
                multi_workspace.workspace().entity_id(),
                workspace.entity_id()
            );
            assert!(
                !multi_workspace
                    .workspaces()
                    .any(|candidate| candidate == &workspace)
            );
        });
    }
}
