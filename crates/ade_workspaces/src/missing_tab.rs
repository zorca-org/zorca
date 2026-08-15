//! The tab a layout gets when this client cannot render the real one.
//!
//! **A layout is the daemon's, and rendering it is not allowed to edit it.** A
//! `Tab::Editor` names a path the daemon never looks inside; if the file has
//! been deleted, moved, or belongs to a worktree this window does not have, the
//! client still has to show *something* — because a tab silently dropped here
//! would be captured back as a layout without it, and one client's missing file
//! would delete the tab for every other client too.
//!
//! **The same is true of a session that will not attach** (item #145, closing
//! the hole `layout`'s docs used to name). A `Tab::Terminal` whose session is
//! alive but unreachable from *here* — a host this client cannot dial, a daemon
//! that refused — is exactly the editor case with a session id in place of a
//! path: dropping it would delete somebody else's live terminal from the shared
//! arrangement. And it is emphatically **not** a kill: failing to attach is not
//! a control that says "kill", so the session is left running and the tab is
//! left naming it.
//!
//! So the tab stays, holding what it stood in for, and reads back out as the
//! same [`Tab`] it came from. Nothing here retries, and nothing here writes.

use ade_session::Tab;
use gpui::{App, Context, EventEmitter, FocusHandle, Focusable, Task, Window};
use ui::prelude::*;
use workspace::item::{Item, TabContentParams};

/// A tab standing in for one this client could not build, remembering what it
/// was so the layout survives the round trip.
pub struct MissingTab {
    tab: Tab,
    focus_handle: FocusHandle,
    // Closing the placeholder must cancel an attach that has not replaced it yet.
    _materialization: Option<Task<()>>,
}

impl MissingTab {
    /// The placeholder for a file that could not be opened.
    pub fn file(path: impl Into<String>, cx: &mut Context<Self>) -> Self {
        Self::new(Tab::Editor { path: path.into() }, cx)
    }

    /// The placeholder for a session that could not be attached to.
    pub fn session(session_id: impl Into<String>, cx: &mut Context<Self>) -> Self {
        Self::new(
            Tab::Terminal {
                session_id: session_id.into().into(),
            },
            cx,
        )
    }

    fn new(tab: Tab, cx: &mut Context<Self>) -> Self {
        Self {
            tab,
            focus_handle: cx.focus_handle(),
            _materialization: None,
        }
    }

    pub(crate) fn set_materialization(&mut self, task: Task<()>) {
        self._materialization = Some(task);
    }

    /// The tab this one is standing in for — what it captures back as.
    pub fn tab(&self) -> &Tab {
        &self.tab
    }

    /// The identifier being stood in for, whole: a path or a session id.
    fn subject(&self) -> &str {
        match &self.tab {
            Tab::Editor { path } => path,
            Tab::Terminal { session_id } => session_id.as_str(),
        }
    }

    /// What a tab has room for: the last component of a path, or the session id
    /// as it is — an id has no parts to take.
    fn short_name(&self) -> &str {
        match &self.tab {
            Tab::Editor { path } => path
                .rsplit(['/', '\\'])
                .find(|component| !component.is_empty())
                .unwrap_or(path),
            Tab::Terminal { session_id } => session_id.as_str(),
        }
    }

    /// Why this tab is a placeholder, in the user's words.
    fn reason(&self) -> &'static str {
        match &self.tab {
            Tab::Editor { .. } => "This file could not be opened",
            // Said in full because the consequence matters: the session is
            // still running, and closing this tab is not what ends it.
            Tab::Terminal { .. } => "This session could not be attached to. It is still running.",
        }
    }
}

impl EventEmitter<()> for MissingTab {}

impl Focusable for MissingTab {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for MissingTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .items_center()
            .justify_center()
            .gap_1()
            .bg(cx.theme().colors().editor_background)
            .child(Label::new(self.reason()).color(Color::Muted))
            // The subject is a machine identifier, so it is set in the mono
            // face the design rules reserve for one.
            .child(
                Label::new(self.subject().to_owned())
                    .buffer_font(cx)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }
}

impl Item for MissingTab {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.short_name().to_owned().into()
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, _cx: &App) -> AnyElement {
        Label::new(self.short_name().to_owned())
            .color(if params.selected {
                Color::Default
            } else {
                Color::Muted
            })
            .strikethrough()
            .into_any_element()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(format!("{}: {}", self.reason(), self.subject()).into())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Entity;

    fn missing_file(path: &str, cx: &mut App) -> Entity<MissingTab> {
        let path = path.to_owned();
        cx.new(|cx| MissingTab::file(path, cx))
    }

    #[gpui::test]
    fn test_the_tab_shows_the_file_name_and_keeps_the_whole_path(cx: &mut gpui::TestAppContext) {
        let item = cx.update(|cx| missing_file("/repos/zed/src/main.rs", cx));
        cx.update(|cx| {
            let item = item.read(cx);
            assert_eq!(
                item.tab(),
                &Tab::Editor {
                    path: "/repos/zed/src/main.rs".to_owned()
                }
            );
            assert_eq!(item.tab_content_text(0, cx), "main.rs");
        });
    }

    #[gpui::test]
    fn test_a_path_with_no_components_is_shown_whole(cx: &mut gpui::TestAppContext) {
        let item = cx.update(|cx| missing_file("/", cx));
        cx.update(|cx| assert_eq!(item.read(cx).tab_content_text(0, cx), "/"));
        // A Windows path is split on its own separator too.
        let item = cx.update(|cx| missing_file(r"C:\repos\zed\main.rs", cx));
        cx.update(|cx| assert_eq!(item.read(cx).tab_content_text(0, cx), "main.rs"));
    }

    /// A placeholder has nothing to save — it stands in for a tab this client
    /// could not build, and the thing it stands for is elsewhere. Closing a
    /// window full of them must not ask about saving changes in files.
    #[gpui::test]
    fn test_a_placeholder_is_never_unsaved_work(cx: &mut gpui::TestAppContext) {
        let file = cx.update(|cx| missing_file("/repos/zed/src/main.rs", cx));
        let session = cx.update(|cx| cx.new(|cx| MissingTab::session("9f2c-terminal", cx)));
        cx.update(|cx| {
            assert!(!file.read(cx).is_dirty(cx));
            assert!(!session.read(cx).is_dirty(cx));
            assert!(!file.read(cx).has_conflict(cx));
        });
    }

    #[gpui::test]
    fn test_a_session_placeholder_keeps_the_session_it_stands_for(cx: &mut gpui::TestAppContext) {
        let item = cx.update(|cx| cx.new(|cx| MissingTab::session("9f2c-terminal", cx)));
        cx.update(|cx| {
            let item = item.read(cx);
            assert_eq!(
                item.tab(),
                &Tab::Terminal {
                    session_id: "9f2c-terminal".into()
                }
            );
            // An id has no components, so the tab shows it whole.
            assert_eq!(item.tab_content_text(0, cx), "9f2c-terminal");
            // And the tab says the session is still running, because a tab that
            // could not be attached to must not read as one that died.
            assert!(
                item.tab_tooltip_text(cx)
                    .is_some_and(|tooltip| tooltip.contains("still running")),
                "the tooltip must not imply the session is gone"
            );
        });
    }
}
