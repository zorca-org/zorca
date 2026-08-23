use std::{path::Path, sync::Arc};

use anyhow::Context as _;
use gpui::{AppContext, Entity, EventEmitter, FocusHandle, Focusable, TaskExt};
use project::{Project, ProjectPath};
use ui::{
    App, Button, ButtonCommon, ButtonStyle, Clickable, Context, Disableable, FluentBuilder,
    InteractiveElement, KeyBinding, Label, LabelCommon, LabelSize, ParentElement, Render,
    SharedString, Styled as _, Window, h_flex, v_flex,
};
use zed_actions::workspace::OpenWithSystem;

use crate::Item;

/// A view to display when a certain buffer/image/other item fails to open.
#[derive(Debug)]
pub struct InvalidItemView {
    /// Which path was attempted to open.
    pub abs_path: Arc<Path>,
    /// An error message, happened when opening the item.
    pub error: SharedString,
    project: Entity<Project>,
    project_path: ProjectPath,
    is_local: bool,
    is_downloading: bool,
    download_message: Option<SharedString>,
    focus_handle: FocusHandle,
}

impl InvalidItemView {
    pub fn new(
        project: Entity<Project>,
        project_path: ProjectPath,
        abs_path: &Path,
        is_local: bool,
        e: &anyhow::Error,
        _: &mut Window,
        cx: &mut App,
    ) -> Self {
        Self {
            project,
            project_path,
            is_local,
            abs_path: Arc::from(abs_path),
            error: format!("{}", e.root_cause()).into(),
            is_downloading: false,
            download_message: None,
            focus_handle: cx.focus_handle(),
        }
    }

    fn download_to_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_downloading {
            return;
        }

        let destination = cx.prompt_for_new_path(
            &dirs::download_dir()
                .or_else(dirs::home_dir)
                .unwrap_or_default(),
            self.abs_path.file_name().and_then(|name| name.to_str()),
        );
        let project = self.project.clone();
        let project_path = self.project_path.clone();

        self.is_downloading = true;
        self.download_message = None;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let destination = match destination
                .await
                .context("failed to receive the download destination")
                .and_then(|result| result.context("failed to choose the download destination"))
            {
                Ok(Some(destination)) => destination,
                Ok(None) => {
                    this.update(cx, |this, cx| {
                        this.is_downloading = false;
                        cx.notify();
                    })?;
                    return anyhow::Ok(());
                }
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.is_downloading = false;
                        this.download_message = Some(format!("Download failed: {error:#}").into());
                        cx.notify();
                    })?;
                    return anyhow::Ok(());
                }
            };

            let result = project
                .update(cx, |project, cx| {
                    project.download_file(
                        project_path.worktree_id,
                        project_path.path,
                        destination.clone(),
                        cx,
                    )
                })
                .await;

            this.update(cx, |this, cx| {
                this.is_downloading = false;
                this.download_message = Some(match result {
                    Ok(()) => format!("Saved to {}", destination.display()).into(),
                    Err(error) => format!("Download failed: {error:#}").into(),
                });
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn play_remote_video(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_downloading {
            return;
        }

        let Some(file_name) = self.abs_path.file_name() else {
            self.download_message = Some("Could not determine the video file name".into());
            cx.notify();
            return;
        };
        // ponytail: keep copies in OS temp so external players can outlive this view;
        // add eviction if repeated playback causes measurable disk growth.
        let directory = std::env::temp_dir().join("zorca-remote-media");
        let destination = directory.join(format!(
            "{}-{}",
            uuid::Uuid::new_v4(),
            file_name.to_string_lossy()
        ));
        let project = self.project.clone();
        let project_path = self.project_path.clone();

        self.is_downloading = true;
        self.download_message = None;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let create_directory = cx.background_spawn({
                let directory = directory.clone();
                async move {
                    std::fs::create_dir_all(&directory).with_context(|| {
                        format!(
                            "failed to create temporary directory {}",
                            directory.display()
                        )
                    })
                }
            });
            let result = async {
                create_directory.await?;
                project
                    .update(cx, |project, cx| {
                        project.download_file(
                            project_path.worktree_id,
                            project_path.path,
                            destination.clone(),
                            cx,
                        )
                    })
                    .await
            }
            .await;

            if result.is_ok() {
                cx.update(|_, cx| cx.open_with_system(&destination))?;
            }

            this.update(cx, |this, cx| {
                this.is_downloading = false;
                this.download_message = match result {
                    Ok(()) => None,
                    Err(error) => Some(format!("Could not play video: {error:#}").into()),
                };
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }
}

fn is_video_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "avi" | "m4v" | "mkv" | "mov" | "mp4" | "webm" | "wmv"
            )
        })
}

impl Item for InvalidItemView {
    type Event = ();

    fn tab_content_text(&self, mut detail: usize, _: &App) -> SharedString {
        // Ensure we always render at least the filename.
        detail += 1;

        let path = self.abs_path.as_ref();

        let mut prefix = path;
        while detail > 0 {
            if let Some(parent) = prefix.parent() {
                prefix = parent;
                detail -= 1;
            } else {
                break;
            }
        }

        let path = if detail > 0 {
            path
        } else {
            path.strip_prefix(prefix).unwrap_or(path)
        };

        SharedString::new(path.to_string_lossy())
    }
}

impl EventEmitter<()> for InvalidItemView {}

impl Focusable for InvalidItemView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for InvalidItemView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let abs_path = self.abs_path.clone();
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle(cx))
            .flex_none()
            .justify_center()
            .overflow_hidden()
            .key_context("InvalidItem")
            .child(
                h_flex().size_full().justify_center().child(
                    v_flex()
                        .justify_center()
                        .gap_2()
                        .child(h_flex().justify_center().child("Could not open file"))
                        .child(
                            h_flex()
                                .justify_center()
                                .child(Label::new(self.error.clone()).size(LabelSize::Small)),
                        )
                        .when(self.is_local, |contents| {
                            contents.child(
                                h_flex().justify_center().child(
                                    Button::new("open-with-system", "Open in Default App")
                                        .on_click(move |_, _, cx| {
                                            cx.open_with_system(&abs_path);
                                        })
                                        .style(ButtonStyle::Outlined)
                                        .key_binding(KeyBinding::for_action(&OpenWithSystem, cx)),
                                ),
                            )
                        })
                        .when(!self.is_local, |contents| {
                            contents
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .justify_center()
                                        .when(is_video_file(&self.abs_path), |buttons| {
                                            buttons.child(
                                                Button::new(
                                                    "play-remote-video",
                                                    "Play in Default App",
                                                )
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.play_remote_video(window, cx);
                                                }))
                                                .disabled(self.is_downloading)
                                                .loading(self.is_downloading)
                                                .style(ButtonStyle::Filled),
                                            )
                                        })
                                        .child(
                                            Button::new("download-to-host", "Download to Host…")
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.download_to_host(window, cx);
                                                }))
                                                .disabled(self.is_downloading)
                                                .style(ButtonStyle::Outlined),
                                        ),
                                )
                                .when_some(self.download_message.clone(), |contents, message| {
                                    contents.child(h_flex().justify_center().child(
                                        Label::new(message).size(LabelSize::Small).buffer_font(cx),
                                    ))
                                })
                        }),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{InvalidItemView, is_video_file};
    use anyhow::anyhow;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::{Project, ProjectPath, WorktreeId};
    use std::path::{Path, PathBuf};
    use util::rel_path::rel_path;

    #[test]
    fn identifies_video_files_case_insensitively() {
        assert!(is_video_file(Path::new("demo.MP4")));
        assert!(is_video_file(Path::new("demo.webm")));
        assert!(!is_video_file(Path::new("demo.png")));
    }

    #[gpui::test]
    async fn remote_download_uses_host_save_dialog_and_surfaces_errors(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);
        let project = Project::test(FakeFs::new(cx.executor()), None, cx).await;
        let project_path = ProjectPath {
            worktree_id: WorktreeId::from_proto(1),
            path: rel_path("reel.mp4").into(),
        };
        let error = anyhow!("Binary files are not supported");
        let (view, window_context) = cx.add_window_view(move |window, cx| {
            InvalidItemView::new(
                project,
                project_path,
                Path::new("/remote/reel.mp4"),
                false,
                &error,
                window,
                cx,
            )
        });

        view.update_in(window_context, |view, window, cx| {
            view.download_to_host(window, cx);
        });
        assert!(cx.did_prompt_for_new_path());

        cx.simulate_new_path_selection(|_| Some(PathBuf::from("/tmp/reel.mp4")));
        cx.run_until_parked();

        view.read_with(cx, |view, _| {
            assert_eq!(
                view.download_message.as_deref(),
                Some("Download failed: not a remote project")
            );
            assert!(!view.is_downloading);
        });
    }
}
