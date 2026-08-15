use crate::{CommonAnimationExt, DiffStat, GradientFade, HighlightedLabel, Tooltip, prelude::*};

use gpui::{
    Animation, AnimationExt, ClickEvent, Hsla, MouseButton, SharedString,
    WindowBackgroundAppearance, pulsating_between,
};
use itertools::Itertools as _;
use std::{path::PathBuf, sync::Arc, time::Duration};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentThreadStatus {
    #[default]
    Completed,
    Running,
    WaitingForConfirmation,
    Error,
}

/// The word a compact row shows next to its status indicator. A finished turn
/// the user has not looked at (`notified`) is blocked on the user just as an
/// explicit confirmation request is, so the two read the same.
pub fn agent_thread_status_word(status: AgentThreadStatus, notified: bool) -> &'static str {
    match status {
        AgentThreadStatus::Error => "Error",
        AgentThreadStatus::Running => "Working",
        AgentThreadStatus::WaitingForConfirmation => "Needs input",
        _ if notified => "Needs input",
        _ => "Idle",
    }
}

/// Text variants of the status hues: the saturated hue stays on the icon, so
/// the word blends it toward the theme's text color until it clears the 4.5:1
/// bar against both the panel and the selected row. Never reuse a dot colour
/// as text.
fn status_word_color(hue: Hsla, cx: &App) -> Color {
    Color::Custom(hue.blend(cx.theme().colors().text.opacity(0.55)))
}

/// The phrase a session's status tooltip states. `needs_input` covers the same
/// two cases [`agent_thread_status_word`] does.
pub fn agent_thread_status_text(status: AgentThreadStatus, needs_input: bool) -> &'static str {
    match status {
        AgentThreadStatus::Error => "Thread has an error",
        AgentThreadStatus::Running => "Working…",
        _ if needs_input => "Waiting for your input",
        _ => "Idle",
    }
}

/// The tooltip a session carries. Naming the session is only worth a line of
/// tooltip where more than one of them is in play, so the prefix is the
/// caller's call.
pub fn agent_thread_status_tooltip(
    status: AgentThreadStatus,
    needs_input: bool,
    prefix: Option<&SharedString>,
) -> SharedString {
    let text = agent_thread_status_text(status, needs_input);
    match prefix {
        Some(prefix) => format!("{prefix} — {text}").into(),
        None => text.into(),
    }
}

/// How large [`agent_thread_status_indicator`] draws: the slot a compact row
/// leads with, or the miniature a collapsed group packs several of into one
/// status cluster.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentThreadStatusIndicatorSize {
    #[default]
    Row,
    Miniature,
}

impl AgentThreadStatusIndicatorSize {
    fn icon_size(self) -> IconSize {
        match self {
            Self::Row => IconSize::XSmall,
            Self::Miniature => IconSize::Indicator,
        }
    }

    /// The filled needs-input dot and the hollow idle circle, in that order.
    /// They differ by a pixel so the two read apart at a glance.
    fn dot_sizes(self) -> (Pixels, Pixels) {
        match self {
            Self::Row => (px(7.), px(6.)),
            Self::Miniature => (px(6.), px(5.)),
        }
    }
}

/// The three indicators that say what a session is doing: a spinning ring
/// while the agent works, a pulsing dot while it is blocked on the user, a
/// hollow circle while it is idle. Single-sourced so the compact row, the
/// divided workspace row and the collapsed group's cluster share one
/// vocabulary.
///
/// Both animations go through `with_animation`, which collapses to a single
/// static frame when `App::reduce_motion` is set; the states stay
/// distinguishable by shape and color without motion.
pub fn agent_thread_status_indicator(
    id: SharedString,
    status: AgentThreadStatus,
    needs_input: bool,
    idle_color: Color,
    size: AgentThreadStatusIndicatorSize,
    cx: &App,
) -> AnyElement {
    let (needs_input_dot, idle_dot) = size.dot_sizes();
    match status {
        AgentThreadStatus::Error => Icon::new(IconName::Close)
            .size(size.icon_size())
            .color(Color::Error)
            .into_any_element(),
        AgentThreadStatus::Running => Icon::new(IconName::LoadCircle)
            .size(size.icon_size())
            .color(Color::Info)
            .with_keyed_rotate_animation(SharedString::from(format!("{id}-working")), 1)
            .into_any_element(),
        _ if needs_input => {
            let dot_color = Color::Warning.color(cx);
            div()
                .size(needs_input_dot)
                .rounded_full()
                .bg(dot_color)
                .with_animation(
                    SharedString::from(format!("{id}-needs-input")),
                    Animation::new(Duration::from_millis(1800))
                        .repeat()
                        .with_easing(pulsating_between(0.45, 1.0)),
                    move |this, delta| this.bg(dot_color.opacity(delta)),
                )
                .into_any_element()
        }
        _ => div()
            .size(idle_dot)
            .rounded_full()
            .border_1()
            .border_color(idle_color.color(cx))
            .into_any_element(),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorktreeKind {
    #[default]
    Main,
    Linked,
}

#[derive(Clone, Default)]
pub struct ThreadItemWorktreeInfo {
    pub worktree_name: Option<SharedString>,
    pub branch_name: Option<SharedString>,
    pub full_path: SharedString,
    pub highlight_positions: Vec<usize>,
    pub kind: WorktreeKind,
}

#[derive(IntoElement, RegisterComponent)]
pub struct ThreadItem {
    id: ElementId,
    icon: IconName,
    icon_char: Option<SharedString>,
    icon_color: Option<Color>,
    icon_visible: bool,
    custom_icon_from_external_svg: Option<SharedString>,
    title: SharedString,
    title_slot: Option<AnyElement>,
    title_label_color: Option<Color>,
    title_generating: bool,
    highlight_positions: Vec<usize>,
    timestamp: SharedString,
    notified: bool,
    status: AgentThreadStatus,
    selected: bool,
    focused: bool,
    hovered: bool,
    rounded: bool,
    compact: bool,
    is_truncated: bool,
    show_status_word: bool,
    status_tooltip_prefix: Option<SharedString>,
    added: Option<usize>,
    removed: Option<usize>,
    project_paths: Option<Arc<[PathBuf]>>,
    project_name: Option<SharedString>,
    worktrees: Vec<ThreadItemWorktreeInfo>,
    is_remote: bool,
    archived: bool,
    on_click: Option<Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
    on_hover: Box<dyn Fn(&bool, &mut Window, &mut App) + 'static>,
    action_slot: Option<AnyElement>,
    base_bg: Option<Hsla>,
}

impl ThreadItem {
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            icon: IconName::ZedAgent,
            icon_char: None,
            icon_color: None,
            icon_visible: true,
            custom_icon_from_external_svg: None,
            title: title.into(),
            title_slot: None,
            title_label_color: None,
            title_generating: false,
            highlight_positions: Vec::new(),
            timestamp: "".into(),
            notified: false,
            status: AgentThreadStatus::default(),
            selected: false,
            focused: false,
            hovered: false,
            rounded: false,
            compact: false,
            is_truncated: true,
            show_status_word: false,
            status_tooltip_prefix: None,
            added: None,
            removed: None,
            project_paths: None,
            project_name: None,
            worktrees: Vec::new(),
            is_remote: false,
            archived: false,
            on_click: None,
            on_hover: Box::new(|_, _, _| {}),
            action_slot: None,
            base_bg: None,
        }
    }

    pub fn timestamp(mut self, timestamp: impl Into<SharedString>) -> Self {
        self.timestamp = timestamp.into();
        self
    }

    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = icon;
        self
    }

    /// Renders the given character in place of the icon. Takes precedence over
    /// [`Self::icon`] and [`Self::custom_icon_from_external_svg`].
    pub fn icon_char(mut self, icon_char: impl Into<SharedString>) -> Self {
        self.icon_char = Some(icon_char.into());
        self
    }

    pub fn icon_color(mut self, color: Color) -> Self {
        self.icon_color = Some(color);
        self
    }

    pub fn icon_visible(mut self, visible: bool) -> Self {
        self.icon_visible = visible;
        self
    }

    pub fn custom_icon_from_external_svg(mut self, svg: impl Into<SharedString>) -> Self {
        self.custom_icon_from_external_svg = Some(svg.into());
        self
    }

    pub fn notified(mut self, notified: bool) -> Self {
        self.notified = notified;
        self
    }

    pub fn status(mut self, status: AgentThreadStatus) -> Self {
        self.status = status;
        self
    }

    pub fn title_generating(mut self, generating: bool) -> Self {
        self.title_generating = generating;
        self
    }

    pub fn title_label_color(mut self, color: Color) -> Self {
        self.title_label_color = Some(color);
        self
    }

    pub fn title_slot(mut self, element: impl IntoElement) -> Self {
        self.title_slot = Some(element.into_any_element());
        self
    }

    pub fn highlight_positions(mut self, positions: Vec<usize>) -> Self {
        self.highlight_positions = positions;
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    pub fn added(mut self, added: usize) -> Self {
        self.added = Some(added);
        self
    }

    pub fn removed(mut self, removed: usize) -> Self {
        self.removed = Some(removed);
        self
    }

    pub fn project_paths(mut self, paths: Arc<[PathBuf]>) -> Self {
        self.project_paths = Some(paths);
        self
    }

    pub fn project_name(mut self, name: impl Into<SharedString>) -> Self {
        self.project_name = Some(name.into());
        self
    }

    pub fn worktrees(mut self, worktrees: Vec<ThreadItemWorktreeInfo>) -> Self {
        self.worktrees = worktrees;
        self
    }

    pub fn is_remote(mut self, is_remote: bool) -> Self {
        self.is_remote = is_remote;
        self
    }

    pub fn archived(mut self, archived: bool) -> Self {
        self.archived = archived;
        self
    }

    pub fn hovered(mut self, hovered: bool) -> Self {
        self.hovered = hovered;
        self
    }

    pub fn rounded(mut self, rounded: bool) -> Self {
        self.rounded = rounded;
        self
    }

    /// Collapses the row to a single line: the metadata line is dropped and the
    /// diff stats and timestamp move inline to the right of the title.
    pub fn compact(mut self, compact: bool) -> Self {
        self.compact = compact;
        self
    }

    pub fn is_truncated(mut self, is_truncated: bool) -> Self {
        self.is_truncated = is_truncated;
        self
    }

    /// Shows the status word ("Working", "Needs input", …) to the left of the
    /// timestamp. Compact rows only — the roomier layout states the status in
    /// its metadata line instead.
    pub fn show_status_word(mut self, show: bool) -> Self {
        self.show_status_word = show;
        self
    }

    /// Names the session the status tooltip is about, as `<Agent>` or
    /// `<Agent> #<instance>`; the tooltip then reads "<prefix> — <status>".
    pub fn status_tooltip_prefix(mut self, prefix: impl Into<SharedString>) -> Self {
        self.status_tooltip_prefix = Some(prefix.into());
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    pub fn on_hover(mut self, on_hover: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_hover = Box::new(on_hover);
        self
    }

    pub fn action_slot(mut self, element: impl IntoElement) -> Self {
        self.action_slot = Some(element.into_any_element());
        self
    }

    pub fn base_bg(mut self, color: Hsla) -> Self {
        self.base_bg = Some(color);
        self
    }
}

impl RenderOnce for ThreadItem {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let color = cx.theme().colors();
        // The fade gradient paints a solid color over the title to blend it into
        // the row background, but a transparent window has no opaque surface to
        // fade into, so it renders as a visible patch; truncate the title instead.
        let opaque_window =
            cx.theme().window_background_appearance() == WindowBackgroundAppearance::Opaque;
        let sidebar_base_bg = color
            .title_bar_background
            .blend(color.panel_background.opacity(0.25));

        let raw_bg = self.base_bg.unwrap_or(sidebar_base_bg);
        let apparent_bg = color.background.blend(raw_bg);

        let base_bg = if self.selected {
            apparent_bg.blend(color.element_active)
        } else {
            apparent_bg
        };

        let hover_color = color
            .element_active
            .blend(color.element_background.opacity(0.2));
        let hover_bg = apparent_bg.blend(hover_color);

        let gradient_overlay = GradientFade::new(base_bg, hover_bg, hover_bg)
            .width(px(64.0))
            .right(px(-10.0))
            .gradient_stop(0.7)
            .group_name("thread-item");

        let separator_color = Color::Custom(color.text_muted.opacity(0.4));
        let dot_separator = || {
            Label::new("•")
                .size(LabelSize::Small)
                .color(separator_color)
        };

        let icon_id = format!("icon-{}", self.id);
        let icon_visible = self.icon_visible;
        let icon_container = || {
            h_flex()
                .id(icon_id.clone())
                .size_4()
                .flex_none()
                .justify_center()
                .when(!icon_visible, |this| this.invisible())
        };
        let compact = self.compact;
        let icon_color = self.icon_color.unwrap_or(Color::Muted);
        // Compact rows lead with a status indicator instead of an agent glyph
        // (see `compact_status_slot` below), so the glyph is only built for the
        // roomier default layout.
        let agent_icon = if let Some(icon_char) = self.icon_char {
            Label::new(icon_char)
                .size(LabelSize::Small)
                .color(icon_color)
                .into_any_element()
        } else if let Some(custom_svg) = self.custom_icon_from_external_svg {
            Icon::from_external_svg(custom_svg)
                .color(icon_color)
                .size(IconSize::Small)
                .into_any_element()
        } else {
            Icon::new(self.icon)
                .color(icon_color)
                .size(IconSize::Small)
                .into_any_element()
        };

        let status = self.status;
        // "Needs input" covers both the agent explicitly asking for
        // confirmation and a finished turn the user has not looked at yet.
        let needs_input = status == AgentThreadStatus::WaitingForConfirmation || self.notified;
        let status_tooltip =
            agent_thread_status_tooltip(status, needs_input, self.status_tooltip_prefix.as_ref());

        let status_icon = if self.status == AgentThreadStatus::Error {
            Some(
                Icon::new(IconName::Close)
                    .size(IconSize::Small)
                    .color(Color::Error),
            )
        } else if self.status == AgentThreadStatus::WaitingForConfirmation {
            Some(
                Icon::new(IconName::Warning)
                    .size(IconSize::XSmall)
                    .color(Color::Warning),
            )
        } else if self.notified {
            Some(
                Icon::new(IconName::Circle)
                    .size(IconSize::Small)
                    .color(Color::Accent),
            )
        } else {
            None
        };

        // A compact row is a live agent session in a tree, so it leads with a
        // status indicator rather than an agent glyph. The slot is a fixed
        // 12x12 box so titles stay in one column across all of the states.
        let compact_status_slot = compact.then(|| {
            let indicator = agent_thread_status_indicator(
                SharedString::from(icon_id.clone()),
                status,
                needs_input,
                icon_color,
                AgentThreadStatusIndicatorSize::Row,
                cx,
            );

            h_flex()
                .id(icon_id.clone())
                .size_3()
                .flex_none()
                .items_center()
                .justify_center()
                .when(!icon_visible, |this| this.invisible())
                .child(indicator)
                .tooltip(Tooltip::text(status_tooltip))
                .into_any_element()
        });

        let icon = if let Some(slot) = compact_status_slot {
            slot
        } else if self.status == AgentThreadStatus::Running {
            icon_container()
                .child(
                    Icon::new(IconName::LoadCircle)
                        .size(IconSize::Small)
                        .color(Color::Muted)
                        .with_rotate_animation(2),
                )
                .into_any_element()
        } else if let Some(status_icon) = status_icon {
            icon_container().child(status_icon).into_any_element()
        } else {
            icon_container().child(agent_icon).into_any_element()
        };

        let title = self.title;
        let highlight_positions = self.highlight_positions;

        let title_label = if let Some(title_slot) = self.title_slot {
            title_slot
        } else if self.title_generating {
            Label::new(title)
                .color(Color::Muted)
                .when(compact, |label| label.size(LabelSize::Small))
                .with_animation(
                    "generating-title",
                    Animation::new(Duration::from_secs(2))
                        .repeat()
                        .with_easing(pulsating_between(0.4, 0.8)),
                    |label, delta| label.alpha(delta),
                )
                .into_any_element()
        } else if highlight_positions.is_empty() {
            Label::new(title)
                .when_some(self.title_label_color, |label, color| label.color(color))
                .when(compact, |label| label.size(LabelSize::Small))
                .when(!opaque_window, |label| label.truncate())
                .into_any_element()
        } else {
            HighlightedLabel::new(title, highlight_positions)
                .when_some(self.title_label_color, |label, color| label.color(color))
                .when(compact, |label| label.size(LabelSize::Small))
                .when(!opaque_window, |label| label.truncate())
                .into_any_element()
        };

        let has_action_slot = self.action_slot.is_some();

        let has_diff_stats = self.added.is_some() || self.removed.is_some();
        let diff_stat_id = self.id.clone();
        // `DiffStat::new` consumes the id, and compact mode renders the stat in a
        // different place, so each site needs its own copy.
        let inline_diff_stat_id = self.id.clone();
        let added_count = self.added.unwrap_or(0);
        let removed_count = self.removed.unwrap_or(0);

        let project_paths = self.project_paths.as_ref().and_then(|paths| {
            let paths_str = paths
                .as_ref()
                .iter()
                .filter_map(|p| p.file_name())
                .filter_map(|name| name.to_str())
                .join(", ");
            if paths_str.is_empty() {
                None
            } else {
                Some(paths_str)
            }
        });

        let has_project_name = self.project_name.is_some();
        let has_project_paths = project_paths.is_some();
        let has_timestamp = !self.timestamp.is_empty();
        let timestamp = self.timestamp;

        // Compact rows carry the tooltip on the status slot itself, so the
        // whole-row tooltip would only ever duplicate it.
        let show_tooltip = !compact
            && matches!(
                self.status,
                AgentThreadStatus::Error | AgentThreadStatus::WaitingForConfirmation
            );

        let linked_worktrees: Vec<ThreadItemWorktreeInfo> = self
            .worktrees
            .into_iter()
            .filter(|wt| wt.kind == WorktreeKind::Linked)
            .filter(|wt| wt.worktree_name.is_some() || wt.branch_name.is_some())
            .collect();

        let has_worktree = !linked_worktrees.is_empty();

        let has_metadata = !compact
            && (has_project_name
                || has_project_paths
                || has_worktree
                || has_diff_stats
                || has_timestamp);

        let status_word = (compact && self.show_status_word)
            .then(|| agent_thread_status_word(status, self.notified));

        // The action slot is drawn over the right end of the title row on hover,
        // so the inline metadata yields to it rather than showing through.
        let show_inline_metadata = compact
            && (has_diff_stats || has_timestamp || status_word.is_some())
            && !(self.hovered && has_action_slot);
        let inline_timestamp = timestamp.clone();
        // The timestamp echoes the status slot, so a live thread's relative
        // time reads as part of the indicator rather than as chrome.
        let timestamp_color = match status {
            AgentThreadStatus::Running => Color::Info,
            _ if needs_input => Color::Warning,
            _ => Color::Muted,
        };
        // Idle has no hue to lighten, so it stays on the muted text role.
        let status_word_color = match status {
            AgentThreadStatus::Error => status_word_color(Color::Error.color(cx), cx),
            AgentThreadStatus::Running => status_word_color(Color::Info.color(cx), cx),
            _ if needs_input => status_word_color(Color::Warning.color(cx), cx),
            _ => Color::Muted,
        };

        v_flex()
            .id(self.id.clone())
            .cursor_pointer()
            .group("thread-item")
            .relative()
            .flex_shrink_0()
            .overflow_hidden()
            .w_full()
            .map(|this| if compact { this.py_0p5() } else { this.py_1() })
            .px_1p5()
            .when(self.selected, |s| s.bg(color.element_active))
            .border_1()
            .border_color(gpui::transparent_black())
            .when(self.focused, |s| s.border_color(color.border_focused))
            .when(self.rounded, |s| s.rounded_sm())
            .hover(|s| s.bg(hover_color))
            .on_hover(self.on_hover)
            .child(
                h_flex()
                    .min_w_0()
                    .w_full()
                    .h_6()
                    .gap_2()
                    .justify_between()
                    .child(
                        h_flex()
                            .id("content")
                            .min_w_0()
                            .flex_1()
                            .gap_1p5()
                            .child(icon)
                            .child(title_label),
                    )
                    .when(self.is_truncated && opaque_window, |this| {
                        this.child(gradient_overlay)
                    })
                    // Drawn after the fade so it stays legible over it.
                    .when(show_inline_metadata, |this| {
                        this.child(
                            h_flex()
                                .flex_none()
                                .gap_1p5()
                                .when(has_diff_stats, |this| {
                                    this.child(DiffStat::new(
                                        inline_diff_stat_id,
                                        added_count,
                                        removed_count,
                                    ))
                                })
                                .when(has_diff_stats && has_timestamp, |this| {
                                    this.child(dot_separator())
                                })
                                .children(status_word.map(|word| {
                                    Label::new(word)
                                        .size(LabelSize::Small)
                                        .color(status_word_color)
                                }))
                                .when(has_timestamp, |this| {
                                    this.child(
                                        Label::new(inline_timestamp)
                                            .size(LabelSize::Small)
                                            .color(timestamp_color),
                                    )
                                }),
                        )
                    })
                    .when(self.hovered, |this| {
                        this.when_some(self.action_slot, |this, slot| {
                            this.child(
                                h_flex()
                                    .relative()
                                    .pr_1p5()
                                    .when(opaque_window, |this| {
                                        this.child(
                                            GradientFade::new(base_bg, hover_bg, hover_bg)
                                                .width(px(120.0))
                                                .right(px(8.))
                                                .gradient_stop(0.90)
                                                .group_name("thread-item"),
                                        )
                                    })
                                    .child(slot)
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    }),
                            )
                        })
                    }),
            )
            .when(has_metadata, |this| {
                this.child(
                    h_flex()
                        .gap_1p5()
                        .child(icon_container()) // Icon Spacing
                        .when(self.archived, |this| {
                            this.child(
                                Icon::new(IconName::Archive).size(IconSize::XSmall).color(
                                    Color::Custom(cx.theme().colors().icon_muted.opacity(0.5)),
                                ),
                            )
                        })
                        .when(
                            has_project_name || has_project_paths || has_worktree,
                            |this| {
                                this.when_some(self.project_name, |this, name| {
                                    this.child(
                                        Label::new(name).size(LabelSize::Small).color(Color::Muted),
                                    )
                                })
                                .when(
                                    has_project_name && (has_project_paths || has_worktree),
                                    |this| this.child(dot_separator()),
                                )
                                .when_some(project_paths, |this, paths| {
                                    this.child(
                                        Label::new(paths)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                })
                                .when(has_project_paths && has_worktree, |this| {
                                    this.child(dot_separator())
                                })
                                .children(
                                    linked_worktrees.into_iter().map(|wt| {
                                        let worktree_label = wt.worktree_name.clone().map(|name| {
                                            if wt.highlight_positions.is_empty() {
                                                Label::new(name)
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted)
                                                    .truncate()
                                                    .into_any_element()
                                            } else {
                                                HighlightedLabel::new(
                                                    name,
                                                    wt.highlight_positions.clone(),
                                                )
                                                .size(LabelSize::Small)
                                                .color(Color::Muted)
                                                .truncate()
                                                .into_any_element()
                                            }
                                        });

                                        // When only the branch is shown, lead with a branch icon;
                                        // otherwise keep the worktree icon (which "covers" both the
                                        // worktree and any accompanying branch).
                                        let chip_icon = if wt.worktree_name.is_none()
                                            && wt.branch_name.is_some()
                                        {
                                            IconName::GitBranch
                                        } else {
                                            IconName::GitWorktree
                                        };

                                        let branch_label = wt.branch_name.map(|branch| {
                                            Label::new(branch)
                                                .size(LabelSize::Small)
                                                .color(Color::Muted)
                                                .truncate()
                                                .into_any_element()
                                        });

                                        let show_separator =
                                            worktree_label.is_some() && branch_label.is_some();

                                        h_flex()
                                            .min_w_0()
                                            .gap_0p5()
                                            .child(
                                                Icon::new(chip_icon)
                                                    .size(IconSize::XSmall)
                                                    .color(Color::Muted),
                                            )
                                            .when_some(worktree_label, |this, label| {
                                                this.child(label)
                                            })
                                            .when(show_separator, |this| {
                                                this.child(
                                                    Label::new("/")
                                                        .size(LabelSize::Small)
                                                        .color(separator_color)
                                                        .flex_shrink_0(),
                                                )
                                            })
                                            .when_some(branch_label, |this, label| {
                                                this.child(label)
                                            })
                                    }),
                                )
                            },
                        )
                        .when(
                            (has_project_name || has_project_paths || has_worktree)
                                && (has_diff_stats || has_timestamp),
                            |this| this.child(dot_separator()),
                        )
                        .when(has_diff_stats, |this| {
                            this.child(DiffStat::new(diff_stat_id, added_count, removed_count))
                        })
                        .when(has_diff_stats && has_timestamp, |this| {
                            this.child(dot_separator())
                        })
                        .when(has_timestamp, |this| {
                            this.child(
                                Label::new(timestamp.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        }),
                )
            })
            .when(show_tooltip, |this| {
                let status = self.status;
                this.tooltip(Tooltip::element(move |_, _| match status {
                    AgentThreadStatus::Error => h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::Close)
                                .size(IconSize::Small)
                                .color(Color::Error),
                        )
                        .child(Label::new("Thread has an Error"))
                        .into_any_element(),
                    AgentThreadStatus::WaitingForConfirmation => h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::Small)
                                .color(Color::Warning),
                        )
                        .child(Label::new("Waiting for Confirmation"))
                        .into_any_element(),
                    _ => gpui::Empty.into_any_element(),
                }))
            })
            .when_some(self.on_click, |this, on_click| this.on_click(on_click))
    }
}

impl Component for ThreadItem {
    fn scope() -> ComponentScope {
        ComponentScope::Agent
    }

    fn description() -> &'static str {
        "A row representing an agent thread in a list, showing its title, status, \
        timestamp, and contextual metadata such as worktree and branch information."
    }

    fn preview(_window: &mut Window, cx: &mut App) -> AnyElement {
        let color = cx.theme().colors();
        let bg = color
            .title_bar_background
            .blend(color.panel_background.opacity(0.25));

        let container = || {
            v_flex()
                .w_72()
                .border_1()
                .border_color(color.border_variant)
                .bg(bg)
        };

        let thread_item_examples = vec![
            single_example(
                "Default",
                container()
                    .child(
                        ThreadItem::new("ti-1", "Linking to the Agent Panel Depending on Settings")
                            .icon(IconName::AiOpenAi)
                            .timestamp("15m"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Waiting for Confirmation",
                container()
                    .child(
                        ThreadItem::new("ti-2b", "Execute shell command in terminal")
                            .timestamp("2h")
                            .status(AgentThreadStatus::WaitingForConfirmation),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Error",
                container()
                    .child(
                        ThreadItem::new("ti-2c", "Failed to connect to language server")
                            .timestamp("5h")
                            .status(AgentThreadStatus::Error),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Running Agent",
                container()
                    .child(
                        ThreadItem::new("ti-3", "Add line numbers option to FileEditBlock")
                            .icon(IconName::AiClaude)
                            .timestamp("23h")
                            .status(AgentThreadStatus::Running),
                    )
                    .into_any_element(),
            ),
            single_example(
                "In Worktree",
                container()
                    .child(
                        ThreadItem::new("ti-4", "Add line numbers option to FileEditBlock")
                            .icon(IconName::AiClaude)
                            .timestamp("2w")
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("link-agent-panel".into()),
                                full_path: "link-agent-panel".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Linked,
                                branch_name: None,
                            }]),
                    )
                    .into_any_element(),
            ),
            single_example(
                "With Changes",
                container()
                    .child(
                        ThreadItem::new("ti-5", "Managing user and project settings interactions")
                            .icon(IconName::AiClaude)
                            .timestamp("1mo")
                            .added(10)
                            .removed(3),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Worktree + Changes + Timestamp",
                container()
                    .child(
                        ThreadItem::new("ti-5b", "Full metadata example")
                            .icon(IconName::AiClaude)
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("my-project".into()),
                                full_path: "my-project".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Linked,
                                branch_name: None,
                            }])
                            .added(42)
                            .removed(17)
                            .timestamp("3w"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Worktree + Branch + Changes + Timestamp",
                container()
                    .child(
                        ThreadItem::new("ti-5c", "Full metadata with branch")
                            .icon(IconName::AiClaude)
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("my-project".into()),
                                full_path: "/worktrees/my-project/zed".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Linked,
                                branch_name: Some("feature-branch".into()),
                            }])
                            .added(42)
                            .removed(17)
                            .timestamp("3w"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Long Branch + Changes (truncation)",
                container()
                    .child(
                        ThreadItem::new("ti-5d", "Metadata overflow with long branch name")
                            .icon(IconName::AiClaude)
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("my-project".into()),
                                full_path: "/worktrees/my-project/zed".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Linked,
                                branch_name: Some("fix-very-long-branch-name-here".into()),
                            }])
                            .added(108)
                            .removed(53)
                            .timestamp("2d"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Main Worktree (hidden) + Changes + Timestamp",
                container()
                    .child(
                        ThreadItem::new("ti-5e", "Main worktree branch with diff stats")
                            .icon(IconName::ZedAgent)
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("zed".into()),
                                full_path: "/projects/zed".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Main,
                                branch_name: Some("sidebar-show-branch-name".into()),
                            }])
                            .added(23)
                            .removed(8)
                            .timestamp("5m"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Long Worktree Name (truncation)",
                container()
                    .child(
                        ThreadItem::new("ti-5f", "Thread with a very long worktree name")
                            .icon(IconName::AiClaude)
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some(
                                    "very-long-worktree-name-that-should-truncate".into(),
                                ),
                                full_path: "/worktrees/very-long-worktree-name/zed".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Linked,
                                branch_name: None,
                            }])
                            .timestamp("1h"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Worktree with Search Highlights",
                container()
                    .child(
                        ThreadItem::new("ti-5g", "Filtered thread with highlighted worktree")
                            .icon(IconName::AiClaude)
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("jade-glen".into()),
                                full_path: "/worktrees/jade-glen/zed".into(),
                                highlight_positions: vec![0, 1, 2, 3],
                                kind: WorktreeKind::Linked,
                                branch_name: Some("fix-scrolling".into()),
                            }])
                            .timestamp("3d"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Multiple Worktrees (no branches)",
                container()
                    .child(
                        ThreadItem::new("ti-5h", "Thread spanning multiple worktrees")
                            .icon(IconName::AiClaude)
                            .worktrees(vec![
                                ThreadItemWorktreeInfo {
                                    worktree_name: Some("jade-glen".into()),
                                    full_path: "/worktrees/jade-glen/zed".into(),
                                    highlight_positions: Vec::new(),
                                    kind: WorktreeKind::Linked,
                                    branch_name: None,
                                },
                                ThreadItemWorktreeInfo {
                                    worktree_name: Some("fawn-otter".into()),
                                    full_path: "/worktrees/fawn-otter/zed-slides".into(),
                                    highlight_positions: Vec::new(),
                                    kind: WorktreeKind::Linked,
                                    branch_name: None,
                                },
                            ])
                            .timestamp("2h"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Multiple Worktrees with Branches",
                container()
                    .child(
                        ThreadItem::new("ti-5i", "Multi-root with per-worktree branches")
                            .icon(IconName::ZedAgent)
                            .worktrees(vec![
                                ThreadItemWorktreeInfo {
                                    worktree_name: Some("jade-glen".into()),
                                    full_path: "/worktrees/jade-glen/zed".into(),
                                    highlight_positions: Vec::new(),
                                    kind: WorktreeKind::Linked,
                                    branch_name: Some("fix".into()),
                                },
                                ThreadItemWorktreeInfo {
                                    worktree_name: Some("fawn-otter".into()),
                                    full_path: "/worktrees/fawn-otter/zed-slides".into(),
                                    highlight_positions: Vec::new(),
                                    kind: WorktreeKind::Linked,
                                    branch_name: Some("main".into()),
                                },
                            ])
                            .timestamp("15m"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Project Name + Worktree + Branch",
                container()
                    .child(
                        ThreadItem::new("ti-5j", "Thread with project context")
                            .icon(IconName::AiClaude)
                            .project_name("my-remote-server")
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("jade-glen".into()),
                                full_path: "/worktrees/jade-glen/zed".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Linked,
                                branch_name: Some("feature-branch".into()),
                            }])
                            .timestamp("1d"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Project Paths + Worktree (archive view)",
                container()
                    .child(
                        ThreadItem::new("ti-5k", "Archived thread with folder paths")
                            .icon(IconName::AiClaude)
                            .project_paths(Arc::from(vec![
                                PathBuf::from("/projects/zed"),
                                PathBuf::from("/projects/zed-slides"),
                            ]))
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("jade-glen".into()),
                                full_path: "/worktrees/jade-glen/zed".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Linked,
                                branch_name: Some("feature".into()),
                            }])
                            .timestamp("2mo"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "All Metadata",
                container()
                    .child(
                        ThreadItem::new("ti-5l", "Thread with every metadata field populated")
                            .icon(IconName::ZedAgent)
                            .project_name("remote-dev")
                            .worktrees(vec![ThreadItemWorktreeInfo {
                                worktree_name: Some("my-worktree".into()),
                                full_path: "/worktrees/my-worktree/zed".into(),
                                highlight_positions: Vec::new(),
                                kind: WorktreeKind::Linked,
                                branch_name: Some("main".into()),
                            }])
                            .added(15)
                            .removed(4)
                            .timestamp("8h"),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Focused Item (Keyboard Selection)",
                container()
                    .child(
                        ThreadItem::new("ti-7", "Implement keyboard navigation")
                            .icon(IconName::AiClaude)
                            .timestamp("12h")
                            .focused(true),
                    )
                    .into_any_element(),
            ),
            single_example(
                "Action Slot",
                container()
                    .child(
                        ThreadItem::new("ti-9", "Hover to see action button")
                            .icon(IconName::AiClaude)
                            .timestamp("6h")
                            .hovered(true)
                            .action_slot(
                                IconButton::new("delete", IconName::Trash)
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Muted),
                            ),
                    )
                    .into_any_element(),
            ),
        ];

        example_group(thread_item_examples)
            .vertical()
            .into_any_element()
    }
}
