//! Translating between the daemon's `LayoutDoc` and Zed's centre pane tree.
//!
//! **The daemon is the only restore path for an ADE workspace.** Opening one
//! asks the daemon what the arrangement is and *builds* that — panes, splits,
//! ratios, tabs, which tab is on top, which pane has focus — and every terminal
//! tab is **attached** to the session it names. Nothing here spawns: a session
//! that is gone stays gone, because the daemon prunes dead sessions out of the
//! document itself and a client that invented a replacement would be inventing
//! state the daemon is the source of truth for.
//!
//! **And back.** Splitting, dragging a tab, closing one, reordering, changing
//! which tab is active or which pane has focus — all of it is captured and
//! pushed back as a new `LayoutDoc`, debounced by [`PUSH_DEBOUNCE`] so a drag
//! is one write rather than sixty. The write is guarded by a revision: it must
//! be one past the revision last seen, and losing that race is answered by
//! re-fetching rather than by retrying.
//!
//! **Three shapes, not two.** Zed's [`Member`] tree is n-ary — a pane split
//! twice the same way is one axis with three members — while the document is
//! binary. [`Arrangement`] is the shape in the middle: a pane tree with its
//! leaves numbered and nothing in them, which is what makes the translation
//! testable without a window and what keeps the pane bookkeeping in one place.
//! Folding an n-ary axis into binary splits is [`layout_from_arrangement`];
//! unfolding is [`arrangement_from_layout`], and the two are inverses on any
//! document whose ratios are not degenerate (see [`MIN_SPLIT_RATIO`]).
//!
//! **A client sees its own writes come back.** The daemon excludes the
//! connection that sent an update from the broadcast, but the event stream is a
//! different connection from the control one — so a `LayoutChanged` for one's
//! own write arrives like anyone else's, and is told apart by revision.
//!
//! **A capture only says what this client could render, so it must be able to
//! render everything.** A terminal tab whose session will not attach from here
//! used to be dropped, and the next push then deleted it for every client —
//! the hole this file's docs carried until item #145. It is closed the way the
//! editor case already was: an unattachable session becomes a [`MissingTab`]
//! holding its id, which captures back as the very `Tab::Terminal` it came
//! from. Nothing is dropped, and nothing is killed — failing to attach is not a
//! control that says "kill".
//!
//! **What kills, and what does not.** Closing a terminal tab kills its session
//! ([`LayoutSync::on_workspace_event`]); a workspace-level kill is the
//! sidebar's, and arrives here as [`WorkspaceEvent::Removed`], which stops this
//! window syncing a workspace that no longer exists. Closing a window, closing
//! an editor tab, closing a placeholder, and *dragging* a terminal tab anywhere
//! all kill nothing.

use crate::{
    AdeWorkspace, LayoutEvent, MissingTab, WorkspaceEvent, WorkspaceLayout,
    WorkspaceLifecycleService,
    terminal_pane::{create_session_terminal, open_session_terminal},
};
use ade_session::{LayoutDoc, LayoutNode, SplitDir, Tab};
use anyhow::Result;
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncWindowContext, Axis, Context, Entity, EntityId,
    Focusable, Global, Subscription, Task, TaskExt as _, WeakEntity, Window,
};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use task::TaskId;
use terminal_view::{TerminalId, TerminalView, terminal_panel};
use util::{ResultExt as _, paths::PathStyle};
use workspace::{
    Member, NewCenterTerminal, NewTerminal, OpenTerminal, Pane, PaneAxis, Workspace,
    item::ItemHandle,
};

/// How long a burst of pane mutations is allowed to settle before the layout is
/// pushed.
///
/// A split-ratio drag is a mutation per frame and a tab drag is one per move;
/// pushing each would be a round trip per frame for a picture nobody has
/// finished drawing yet. Long enough to coalesce a gesture, short enough that a
/// second client sees the result while the user still remembers doing it.
pub const PUSH_DEBOUNCE: Duration = Duration::from_millis(300);

/// The narrowest a split is built.
///
/// A document may legally say `0.0`, which is a pane with no width — invisible,
/// and with no divider left to drag it back out by. Rendering that faithfully
/// would be rendering a trap, so the ratio is clamped on the way in. The
/// clamped value is what gets captured back, which is the one place the round
/// trip is deliberately not an identity.
pub const MIN_SPLIT_RATIO: f32 = 0.05;

/// Prefix on the spawn-task id of a terminal attached to a daemon session.
///
/// The session id lives on the terminal itself rather than in a side map, so a
/// tab dragged to another pane — or another window — is still recognisably that
/// session's when the layout is captured.
///
/// Defined in `task` rather than here because `terminal_view` reads it too, to
/// keep a session terminal out of the save prompts a running *task* belongs in
/// ([`TaskId::is_ade_session`]). Two spellings of it would be two answers to
/// "is this a session terminal?".
const SESSION_TASK_PREFIX: &str = task::ADE_SESSION_TASK_PREFIX;

/// The spawn-task id identifying a terminal as one session's.
pub fn session_task_id(session_id: &str) -> TaskId {
    TaskId(format!("{SESSION_TASK_PREFIX}{session_id}"))
}

/// The session a terminal item is attached to, if it is one of ours.
pub(crate) fn session_of_item(item: &dyn ItemHandle, cx: &App) -> Option<String> {
    let terminal_view = item.downcast::<TerminalView>()?;
    let terminal = terminal_view.read(cx).terminal().read(cx);
    let id = &terminal.task()?.spawned_task.id.0;
    id.strip_prefix(SESSION_TASK_PREFIX).map(str::to_owned)
}

fn is_running_terminal(item: &dyn ItemHandle, cx: &App) -> bool {
    item.downcast::<TerminalView>().is_some_and(|terminal| {
        terminal
            .read(cx)
            .terminal()
            .read(cx)
            .task()
            .is_some_and(|task| task.status == terminal::TaskStatus::Running)
    })
}

/// Makes "new terminal" and "open in terminal" mean *daemon* terminals in a
/// window whose centre the daemon owns — for those actions outright, and for
/// the toggle pair in its opening half only ([`toggle_session_terminal`]).
///
/// **This has to be registered before `terminal_view`'s handler for the same
/// actions**, and is: a workspace's action listeners run in registration order
/// in the bubble phase, and `ade_workspaces::init` is the first thing
/// `zed::init` does — well ahead of `terminal_view::init` in `main`. Moving
/// either call changes which handler answers.
///
/// A window the daemon does not own falls through to the stock handler with
/// `cx.propagate()`, so a plain Zed window keeps getting a plain shell.
pub(crate) fn init(cx: &mut App) {
    terminal_panel::on_add_center_terminal(cx, open_new_session_terminal);
    cx.observe_new(|zed_workspace: &mut Workspace, _, _| {
        // Both actions, because ZOrca is terminal-first and `NewTerminal`
        // already opens in the centre (`TerminalPanel::new_terminal`) — two
        // names for the same gesture, and the keymaps bind the second.
        zed_workspace.register_action(|zed_workspace, _: &NewCenterTerminal, window, cx| {
            new_session_terminal(zed_workspace, window, cx);
        });
        zed_workspace.register_action(|zed_workspace, _: &NewTerminal, window, cx| {
            new_session_terminal(zed_workspace, window, cx);
        });
        zed_workspace.register_action(|zed_workspace, action: &OpenTerminal, window, cx| {
            let Some(task) = open_new_session_terminal_at(
                zed_workspace,
                None,
                Some(action.working_directory.clone()),
                window,
                cx,
            ) else {
                cx.propagate();
                return;
            };
            task.detach_and_log_err(cx);
        });
        // The toggle pair means "focus the centre terminal", and only *opens*
        // one when the centre has none — that opening was the one remaining
        // way an ADE window could get a plain local shell in its centre, a
        // shell layout capture silently drops.
        zed_workspace.register_action(|zed_workspace, _: &terminal_panel::Toggle, window, cx| {
            toggle_session_terminal(zed_workspace, window, cx);
        });
        zed_workspace.register_action(
            |zed_workspace, _: &terminal_panel::ToggleFocus, window, cx| {
                toggle_session_terminal(zed_workspace, window, cx);
            },
        );
    })
    .detach();
}

/// [`new_session_terminal`], gated the way the toggle actions need it: a
/// window already showing a terminal propagates instead, because the stock
/// handler's focus-the-existing-terminal half is right as it is.
fn toggle_session_terminal(
    zed_workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let has_terminal = zed_workspace.panes().iter().any(|pane| {
        pane.read(cx)
            .items_of_type::<TerminalView>()
            .next()
            .is_some()
    });
    if has_terminal {
        cx.propagate();
        return;
    }
    new_session_terminal(zed_workspace, window, cx);
}

/// Adds one session to the workspace this window is showing and opens a
/// terminal on it, in the active pane.
///
/// **Nothing here writes the layout.** The tab carries its session id like
/// every other one ([`session_task_id`]), so the window's own capture picks it
/// up and pushes it exactly as a split or a drag would — which is also what
/// makes closing it kill the right session.
///
/// The silent fall-through to the stock handler is for windows ADE does not
/// own, and only those: an ADE-owned window that cannot create a session says
/// so, rather than quietly handing the gesture back to Zed.
fn new_session_terminal(
    zed_workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(task) = open_new_session_terminal(zed_workspace, None, window, cx) else {
        cx.propagate();
        return;
    };
    task.detach_and_log_err(cx);
}

fn open_new_session_terminal(
    zed_workspace: &mut Workspace,
    terminal_id: Option<TerminalId>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Task<Result<WeakEntity<terminal::Terminal>>>> {
    open_new_session_terminal_at(zed_workspace, terminal_id, None, window, cx)
}

fn open_new_session_terminal_at(
    zed_workspace: &mut Workspace,
    terminal_id: Option<TerminalId>,
    working_directory: Option<PathBuf>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Task<Result<WeakEntity<terminal::Terminal>>>> {
    // The sync is where this window's ADE workspace is recorded, so a window
    // without one has nothing to create a session *in* — and a window the
    // daemon does not own has no business holding one.
    if !zed_workspace.ade_owns_layout() {
        return None;
    }
    let Some(sync) = AdeLayouts::sync_for(cx.entity_id(), cx) else {
        let message = "This window owns an ADE layout but is not syncing one, so no session could be created in it.";
        log::error!("{message}");
        zed_workspace.show_error(message, cx);
        return Some(Task::ready(Err(anyhow::anyhow!(message))));
    };

    let ade_workspace = sync.read(cx).ade_workspace.clone();
    let working_directory = working_directory
        .or_else(|| {
            zed_workspace
                .project()
                .read(cx)
                .active_project_directory(cx)
                .map(|path| path.to_path_buf())
        })
        .unwrap_or_else(|| ade_workspace.repository_path.clone());
    let lifecycle = crate::lifecycle_service(cx);
    let pane = zed_workspace.active_pane().downgrade();
    let zed_workspace = zed_workspace.weak_handle();
    // Weak across the await: a strong handle would keep the sync alive past the
    // window it belongs to, and the window's release is what ends it.
    let sync = sync.downgrade();

    Some(cx.spawn_in(window, async move |_, cx| {
        let created = cx
            .background_spawn({
                let ade_workspace = ade_workspace.clone();
                let working_directory = working_directory.clone();
                // Blocking: creating the session and resolving its argv are two
                // round trips to the backend.
                async move {
                    lifecycle.create_session_in_workspace(&ade_workspace, &working_directory)
                }
            })
            .await;
        let (session_id, argv) = match created {
            Ok(created) => created,
            Err(error) => {
                log::error!(
                    "creating another session in ADE workspace {} failed: {error:#}",
                    ade_workspace.id
                );
                let message = format!("{error:#}");
                zed_workspace
                    .update(cx, |zed_workspace, cx| {
                        zed_workspace.show_error(message, cx)
                    })
                    .ok();
                return Err(error);
            }
        };

        // A remote workspace's checkout is a path on *its* host: the attach
        // client is local, and the session's cwd was set where it runs.
        let cwd = (!ade_workspace.is_remote()).then_some(working_directory);
        let terminal_view = open_session_terminal(
            &zed_workspace,
            &pane,
            &session_id,
            &ade_workspace,
            cwd,
            argv,
            None,
            cx,
        )
        .await?;
        if let Some(terminal_id) = terminal_id {
            terminal_view.update(cx, |terminal_view, _| {
                terminal_view.set_terminal_id(terminal_id)
            });
        }

        // Unlike a rendered layout, this tab is a gesture: it is what the user
        // just asked to look at.
        zed_workspace
            .update_in(cx, |zed_workspace, window, cx| {
                let focus = !zed_workspace.has_active_modal(window, cx);
                zed_workspace.activate_item(&terminal_view, true, focus, window, cx);
            })
            .ok();

        sync.update(cx, |sync, cx| {
            // Before the debounced capture can run: the map a closed tab is
            // resolved through has to know this session, or closing it would
            // leave a process nothing can reach.
            sync.remember_sessions(cx);
            sync.schedule(cx);
        })
        .log_err();

        Ok(terminal_view.read_with(cx, |terminal_view, _| terminal_view.terminal().downgrade()))
    }))
}

// ---------------------------------------------------------------------------
// The shape in the middle
// ---------------------------------------------------------------------------

/// One pane's worth of a layout: what is in it and which tab is on top.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leaf {
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub focused: bool,
}

/// A pane tree with its leaves numbered and empty.
///
/// N-ary like Zed's [`Member`], because that is the side that can be, and
/// contentless so that both directions of the translation are pure functions
/// over a shape — see the module docs.
#[derive(Clone, Debug, PartialEq)]
pub enum Arrangement {
    Axis {
        dir: SplitDir,
        /// One per member, summing to the member count — Zed's sizing unit.
        flexes: Vec<f32>,
        members: Vec<Arrangement>,
    },
    /// An index into the leaves the arrangement was built with.
    Pane(usize),
}

/// The two flexes a split `ratio` becomes.
///
/// Zed sizes an axis by a flex per member which must sum to the member count,
/// so a two-way split at `r` is `[2r, 2 - 2r]`. Clamped by
/// [`MIN_SPLIT_RATIO`]; a ratio that is not a number at all is a half.
pub fn split_flexes(ratio: f32) -> Vec<f32> {
    let ratio = if ratio.is_finite() {
        ratio.clamp(MIN_SPLIT_RATIO, 1.0 - MIN_SPLIT_RATIO)
    } else {
        0.5
    };
    vec![2.0 * ratio, 2.0 * (1.0 - ratio)]
}

/// Unfold a document into the pane tree to build and the leaves to fill it
/// with, numbered in tree order.
pub fn arrangement_from_layout(root: &LayoutNode) -> (Arrangement, Vec<Leaf>) {
    fn walk(node: &LayoutNode, leaves: &mut Vec<Leaf>) -> Arrangement {
        match node {
            LayoutNode::Leaf {
                tabs,
                active,
                focused,
            } => {
                leaves.push(Leaf {
                    tabs: tabs.clone(),
                    active: (*active).min(tabs.len().saturating_sub(1)),
                    focused: *focused,
                });
                Arrangement::Pane(leaves.len() - 1)
            }
            LayoutNode::Split {
                dir,
                ratio,
                children,
            } => Arrangement::Axis {
                dir: *dir,
                flexes: split_flexes(*ratio),
                members: vec![walk(&children[0], leaves), walk(&children[1], leaves)],
            },
        }
    }

    let mut leaves = Vec::new();
    let arrangement = walk(root, &mut leaves);
    (arrangement, leaves)
}

/// Fold a pane tree back into the binary document the daemon stores.
///
/// `leaf` says what a numbered pane holds; `None` for a pane holding nothing a
/// layout can name — a scratch buffer, a panel's own item, a pane emptied by
/// the user. Such a pane collapses the way a pruned split does, so the document
/// never comes back with a hole in it, and `None` overall means there was
/// nothing to store at all.
pub fn layout_from_arrangement(
    arrangement: &Arrangement,
    leaf: &mut impl FnMut(usize) -> Option<Leaf>,
) -> Option<LayoutNode> {
    match arrangement {
        Arrangement::Pane(index) => leaf(*index).map(|leaf| LayoutNode::Leaf {
            tabs: leaf.tabs,
            active: leaf.active,
            focused: leaf.focused,
        }),
        Arrangement::Axis {
            dir,
            flexes,
            members,
        } => {
            let kept: Vec<(f32, LayoutNode)> = members
                .iter()
                .enumerate()
                .filter_map(|(index, member)| {
                    let node = layout_from_arrangement(member, leaf)?;
                    Some((flexes.get(index).copied().unwrap_or(1.0), node))
                })
                .collect();
            fold(*dir, kept)
        }
    }
}

/// Fold `n` weighted members into right-nested binary splits.
///
/// The first member's share of the total is the outer ratio and everything
/// after it is the other child, recursively — so a three-member axis at equal
/// flexes comes back as a `1/3` split whose second child is a `1/2` split,
/// which is the same picture.
fn fold(dir: SplitDir, mut members: Vec<(f32, LayoutNode)>) -> Option<LayoutNode> {
    if members.len() <= 1 {
        return members.pop().map(|(_, node)| node);
    }
    let total: f32 = members.iter().map(|(flex, _)| *flex).sum();
    let (flex, first) = members.remove(0);
    let ratio = if total.is_finite() && total > 0.0 {
        flex / total
    } else {
        0.5
    };
    let rest = fold(dir, members)?;
    Some(LayoutNode::Split {
        dir,
        ratio,
        children: Box::new([first, rest]),
    })
}

/// A document's split direction as Zed's axis.
///
/// `Horizontal` means the children sit side by side, which is Zed's horizontal
/// axis; `Vertical` stacks them.
fn axis_of(dir: SplitDir) -> Axis {
    match dir {
        SplitDir::Horizontal => Axis::Horizontal,
        SplitDir::Vertical => Axis::Vertical,
    }
}

fn dir_of(axis: Axis) -> SplitDir {
    match axis {
        Axis::Horizontal => SplitDir::Horizontal,
        Axis::Vertical => SplitDir::Vertical,
    }
}

/// Flexes rescaled to sum to their own count, which is what Zed's axis expects
/// and what pruning a member breaks.
fn normalized(flexes: Vec<f32>) -> Vec<f32> {
    let count = flexes.len() as f32;
    let total: f32 = flexes.iter().sum();
    if !total.is_finite() || total <= 0.0 {
        return vec![1.0; flexes.len()];
    }
    flexes
        .into_iter()
        .map(|flex| flex / total * count)
        .collect()
}

// ---------------------------------------------------------------------------
// Building: document → panes
// ---------------------------------------------------------------------------

/// Builds the window's centre pane tree from `layout`, attaching a terminal to
/// every `Tab::Terminal` and opening every `Tab::Editor` by path.
///
/// Never spawns: the argv for each terminal comes from
/// [`WorkspaceLifecycleService::attach_session_command`], which attaches to a
/// session that exists or fails. Both failures end in a [`MissingTab`] holding
/// what it stood in for — a path, or a session id — so that neither a file this
/// client does not have nor a session it cannot reach deletes a tab from the
/// document everybody shares.
pub fn render_layout(
    zed_workspace: &Entity<Workspace>,
    ade_workspace: &AdeWorkspace,
    layout: LayoutDoc,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<()>> {
    render_layout_if_unchanged(zed_workspace, ade_workspace, layout, None, window, cx)
}

fn render_layout_if_unchanged(
    zed_workspace: &Entity<Workspace>,
    ade_workspace: &AdeWorkspace,
    layout: LayoutDoc,
    expected_current: Option<(LayoutDoc, Entity<Pane>)>,
    window: &mut Window,
    cx: &mut App,
) -> Task<Result<()>> {
    let lifecycle = crate::lifecycle_service(cx);
    let zed_workspace = zed_workspace.downgrade();
    let ade_workspace = ade_workspace.clone();

    window.spawn(cx, async move |cx| {
        let (arrangement, leaves) = arrangement_from_layout(&layout.root);

        if let Some((expected, _)) = expected_current.as_ref() {
            let changed = zed_workspace.read_with(cx, |zed_workspace, cx| {
                capture_persistable_layout(zed_workspace, cx).as_ref() != Some(expected)
            })?;
            if changed {
                return Ok(());
            }
        }

        let mut reusable_terminals = zed_workspace.read_with(cx, |zed_workspace, cx| {
            let mut terminals: HashMap<String, Vec<Box<dyn ItemHandle>>> = HashMap::new();
            for pane in zed_workspace.center().panes() {
                for item in pane.read(cx).items() {
                    if let Some(session_id) = session_of_item(item.as_ref(), cx) {
                        if !is_running_terminal(item.as_ref(), cx) {
                            continue;
                        }
                        terminals.entry(session_id).or_default().push(item.clone());
                    }
                }
            }
            terminals
        })?;

        let mut panes = Vec::with_capacity(leaves.len());
        let mut pending_terminals = Vec::new();
        for leaf in &leaves {
            let pane = zed_workspace.update_in(cx, |zed_workspace, window, cx| {
                zed_workspace.new_center_pane(window, cx).downgrade()
            })?;
            pending_terminals.extend(fill_pane(&zed_workspace, &pane, leaf, cx).await);
            panes.push(pane);
        }
        let committed = zed_workspace.update_in(cx, |zed_workspace, window, cx| {
            let mut created: Vec<Entity<Pane>> =
                panes.iter().filter_map(|pane| pane.upgrade()).collect();
            let mut kept: Vec<Option<Entity<Pane>>> = Vec::with_capacity(panes.len());
            for pane in &panes {
                // An empty pane is one whose every tab failed to open. Zed's
                // own restore drops those rather than rendering a pane with
                // nothing in it, and so does this.
                let pane = pane.upgrade().filter(|pane| pane.read(cx).items_len() > 0);
                kept.push(pane);
            }

            let root = member_of(&arrangement, &kept);
            let focused = leaves
                .iter()
                .position(|leaf| leaf.focused)
                .and_then(|index| kept.get(index).cloned().flatten());
            let mut installed: Vec<Entity<Pane>> = kept.iter().flatten().cloned().collect();

            let root = match root {
                Some(root) => root,
                // Nothing in the document could be rendered. A window still
                // needs a centre, so it gets one empty pane — and the document
                // is left alone, because this client failing to render it is
                // not the same as it being wrong.
                None => {
                    let pane = zed_workspace.new_center_pane(window, cx);
                    installed.push(pane.clone());
                    created.push(pane.clone());
                    Member::Pane(pane)
                }
            };
            if let Some((expected, expected_active_pane)) = expected_current.as_ref() {
                let current = capture_persistable_layout(zed_workspace, cx);
                let current_center_panes = zed_workspace.center().panes();
                let active_pane = current_center_panes
                    .iter()
                    .copied()
                    .find(|pane| pane.read(cx).focus_handle(cx).contains_focused(window, cx))
                    .cloned()
                    .or_else(|| {
                        current_center_panes
                            .iter()
                            .copied()
                            .find(|pane| *pane == expected_active_pane)
                            .cloned()
                    })
                    .unwrap_or_else(|| zed_workspace.center().first_pane());
                let changed = current
                    .as_ref()
                    .is_none_or(|current| !same_layout_except_focus(current, expected))
                    || &active_pane != expected_active_pane;
                if changed {
                    zed_workspace.discard_center_panes(created, active_pane, window, cx);
                    return false;
                }
            }
            let Some(active_pane) = focused.clone().or_else(|| installed.first().cloned()) else {
                return false;
            };
            pending_terminals.retain(|(placeholder, session_id)| {
                let Some(destination) = zed_workspace.pane_for(placeholder) else {
                    return true;
                };
                let Some(index) = destination.read(cx).index_for_item(placeholder) else {
                    return true;
                };
                let Some(terminals) = reusable_terminals.get_mut(session_id) else {
                    return true;
                };
                let Some((source, terminal)) = terminals
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(index, terminal)| {
                        if !is_running_terminal(terminal.as_ref(), cx) {
                            return None;
                        }
                        zed_workspace
                            .pane_for(terminal.as_ref())
                            .map(|pane| (pane, index))
                    })
                    .map(|(source, index)| (source, terminals.remove(index)))
                else {
                    return true;
                };
                let terminal_id = terminal.item_id();
                destination.update(cx, |pane, cx| {
                    pane.add_item(terminal, false, false, Some(index), window, cx);
                    pane.remove_item(placeholder.entity_id(), false, false, window, cx);
                });
                source.update(cx, |pane, cx| {
                    pane.remove_item(terminal_id, false, false, window, cx)
                });
                false
            });
            let unused = created
                .into_iter()
                .filter(|created| !installed.iter().any(|installed| installed == created))
                .collect();
            zed_workspace.discard_center_panes(unused, active_pane, window, cx);
            zed_workspace.set_center_group(root, focused, window, cx);
            activate_layout_tabs(&kept, &leaves, window, cx);
            true
        })?;
        if !committed {
            return Ok(());
        }

        let sessions: Vec<String> = pending_terminals
            .iter()
            .map(|(_, session_id)| session_id.clone())
            .collect();
        let argvs = if sessions.is_empty() {
            HashMap::new()
        } else {
            cx.background_spawn({
                let lifecycle = lifecycle.clone();
                let ade_workspace = ade_workspace.clone();
                async move { attach_argvs(&lifecycle, &ade_workspace, &sessions) }
            })
            .await
        };

        // A remote workspace's checkout is a path on *its* host, so there is no
        // local directory to start a client in — the session's own cwd is the
        // host's, set when it was created.
        let cwd = (!ade_workspace.is_remote()).then(|| ade_workspace.repository_path.clone());
        for (placeholder, session_id) in pending_terminals {
            if let Some(argv) = argvs.get(&session_id) {
                let placeholder_is_open = zed_workspace
                    .read_with(cx, |zed_workspace, _| {
                        zed_workspace.pane_for(&placeholder).is_some()
                    })
                    .unwrap_or(false);
                if !placeholder_is_open {
                    continue;
                }
                let materialization = cx.spawn({
                    let zed_workspace = zed_workspace.clone();
                    let placeholder = placeholder.downgrade();
                    let ade_workspace = ade_workspace.clone();
                    let cwd = cwd.clone();
                    let argv = argv.clone();
                    async move |cx| {
                        let terminal_view = create_session_terminal(
                            &zed_workspace,
                            &session_id,
                            &ade_workspace,
                            cwd,
                            argv,
                            cx,
                        )
                        .await
                        .log_err();
                        if let Some(terminal_view) = terminal_view
                            && let Some(placeholder) = placeholder.upgrade()
                        {
                            replace_terminal_placeholder(
                                &zed_workspace,
                                &placeholder,
                                &terminal_view,
                                cx,
                            )
                            .log_err();
                        }
                    }
                });
                placeholder.update(cx, |placeholder, _| {
                    placeholder.set_materialization(materialization)
                });
            }
        }
        Ok(())
    })
}

fn activate_layout_tabs(
    panes: &[Option<Entity<Pane>>],
    leaves: &[Leaf],
    window: &mut Window,
    cx: &mut App,
) {
    for (pane, leaf) in panes.iter().zip(leaves) {
        let Some(pane) = pane else {
            continue;
        };
        pane.update(cx, |pane, cx| {
            let index = leaf.active.min(pane.items_len().saturating_sub(1));
            pane.activate_item(index, false, false, window, cx);
        });
    }
}

/// One attach argv per session, skipping the ones the backend refuses.
///
/// Blocking, so callers run it on the background executor. A session the
/// backend will not attach to is left out rather than failing the whole render:
/// the rest of the workspace is still worth showing.
fn attach_argvs(
    lifecycle: &WorkspaceLifecycleService,
    ade_workspace: &AdeWorkspace,
    sessions: &[String],
) -> HashMap<String, Vec<String>> {
    let mut argvs = HashMap::with_capacity(sessions.len());
    for session in sessions {
        match lifecycle.attach_session_command(ade_workspace, session) {
            Ok(argv) => {
                argvs.insert(session.clone(), argv);
            }
            Err(error) => {
                log::warn!("cannot attach to session {session}: {error:#}");
            }
        }
    }
    argvs
}

/// Puts one leaf's tabs into one pane, in document order.
async fn fill_pane(
    zed_workspace: &WeakEntity<Workspace>,
    pane: &WeakEntity<Pane>,
    leaf: &Leaf,
    cx: &mut AsyncWindowContext,
) -> Vec<(Entity<MissingTab>, String)> {
    let mut pending_terminals = Vec::new();
    for tab in &leaf.tabs {
        match tab {
            Tab::Terminal { session_id } => {
                // A staging pane must stay side-effect free: starting an attach
                // client here would resize the shared PTY even if the final
                // stale-layout check discards this pane.
                if let Some(placeholder) = add_placeholder(pane, tab.clone(), cx).await {
                    pending_terminals.push((placeholder, session_id.0.clone()));
                }
            }
            Tab::Editor { path } => {
                open_editor_tab(zed_workspace, pane, path, cx).await;
            }
        }
    }
    pending_terminals
}

/// Adds the placeholder that preserves a tab until its real item is ready.
async fn add_placeholder(
    pane: &WeakEntity<Pane>,
    tab: Tab,
    cx: &mut AsyncWindowContext,
) -> Option<Entity<MissingTab>> {
    pane.update_in(cx, |pane, window, cx| {
        let placeholder = cx.new(|cx| match tab {
            Tab::Terminal { session_id } => MissingTab::session(session_id.0, cx),
            Tab::Editor { path } => MissingTab::file(path, cx),
        });
        pane.add_item(
            Box::new(placeholder.clone()),
            false,
            false,
            None,
            window,
            cx,
        );
        placeholder
    })
    .log_err()
}

fn replace_terminal_placeholder(
    zed_workspace: &WeakEntity<Workspace>,
    placeholder: &Entity<MissingTab>,
    terminal: &Entity<TerminalView>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let pane =
        zed_workspace.read_with(cx, |zed_workspace, _| zed_workspace.pane_for(placeholder))?;
    let Some(pane) = pane else {
        return Ok(());
    };
    pane.update_in(cx, |pane, window, cx| {
        let Some(index) = pane.index_for_item(placeholder) else {
            return;
        };
        let active_item = pane.active_item();
        let active_item_was_focused = pane.has_focus(window, cx);
        pane.add_item_inner(
            Box::new(terminal.clone()),
            false,
            false,
            false,
            Some(index),
            window,
            cx,
        );
        pane.remove_item(placeholder.entity_id(), false, false, window, cx);
        let active_index = active_item.as_ref().and_then(|active_item| {
            if active_item.item_id() == placeholder.entity_id() {
                pane.index_for_item(terminal)
            } else {
                pane.index_for_item(active_item.as_ref())
            }
        });
        if let Some(active_index) = active_index {
            pane.activate_item(active_index, false, active_item_was_focused, window, cx);
        }
    })
}

/// Opens one editor tab, or the placeholder standing in for it.
///
/// Two ways a path can fail — it is in no worktree this window has, or it is in
/// one and will not open — and both end in the same [`MissingTab`], so the
/// document keeps the tab it named. See that module for why dropping it would
/// be worse than showing a dead one.
async fn open_editor_tab(
    zed_workspace: &WeakEntity<Workspace>,
    pane: &WeakEntity<Pane>,
    path: &str,
    cx: &mut AsyncWindowContext,
) {
    let project_path = zed_workspace
        .read_with(cx, |zed_workspace, cx| {
            zed_workspace
                .project()
                .read(cx)
                .find_project_path(PathBuf::from(path), cx)
        })
        .ok()
        .flatten();

    if let Some(project_path) = project_path {
        let opened = zed_workspace.update_in(cx, |zed_workspace, window, cx| {
            zed_workspace.open_path(project_path, Some(pane.clone()), false, window, cx)
        });
        if let Ok(opened) = opened
            && opened.await.is_ok()
        {
            return;
        }
    }

    drop(
        add_placeholder(
            pane,
            Tab::Editor {
                path: path.to_owned(),
            },
            cx,
        )
        .await,
    );
}

/// Materialises an [`Arrangement`] as Zed's own tree, given the pane each leaf
/// ended up as — `None` for a leaf that came out empty.
///
/// An axis that loses a member collapses into what is left, exactly as the
/// daemon's own pruning does, and its flexes are rescaled so the survivors keep
/// their relative sizes.
fn member_of(arrangement: &Arrangement, panes: &[Option<Entity<Pane>>]) -> Option<Member> {
    match arrangement {
        Arrangement::Pane(index) => panes.get(*index).cloned().flatten().map(Member::Pane),
        Arrangement::Axis {
            dir,
            flexes,
            members,
        } => {
            let mut kept_flexes = Vec::new();
            let mut kept_members = Vec::new();
            for (index, member) in members.iter().enumerate() {
                let Some(member) = member_of(member, panes) else {
                    continue;
                };
                kept_flexes.push(flexes.get(index).copied().unwrap_or(1.0));
                kept_members.push(member);
            }
            match kept_members.len() {
                0 => None,
                1 => kept_members.pop(),
                _ => Some(Member::Axis(PaneAxis::load(
                    axis_of(*dir),
                    kept_members,
                    Some(normalized(kept_flexes)),
                ))),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Capturing: panes → document
// ---------------------------------------------------------------------------

/// The layout this window is showing, as the daemon would store it.
///
/// `None` when the centre holds nothing a document can name — a window showing
/// only untitled buffers, say — which is a reason not to write rather than a
/// reason to write an empty document over a real one.
pub fn capture_layout(zed_workspace: &Workspace, cx: &App) -> Option<LayoutDoc> {
    let mut panes = Vec::new();
    let arrangement = arrangement_of(&zed_workspace.center().root, &mut panes);
    let active_pane = zed_workspace.active_pane().clone();

    let root = layout_from_arrangement(&arrangement, &mut |index| {
        let pane = panes.get(index)?;
        leaf_of_pane(pane, &active_pane, zed_workspace, cx)
    })?;
    Some(LayoutDoc::new(root))
}

fn capture_persistable_layout(zed_workspace: &Workspace, cx: &App) -> Option<LayoutDoc> {
    capture_layout(zed_workspace, cx).or_else(|| {
        zed_workspace
            .panes()
            .iter()
            .all(|pane| pane.read(cx).items_len() == 0)
            .then(LayoutDoc::empty)
    })
}

/// The shape of Zed's tree, with its panes collected in the order the leaves
/// are numbered.
fn arrangement_of(member: &Member, panes: &mut Vec<Entity<Pane>>) -> Arrangement {
    match member {
        Member::Pane(pane) => {
            panes.push(pane.clone());
            Arrangement::Pane(panes.len() - 1)
        }
        Member::Axis(axis) => Arrangement::Axis {
            dir: dir_of(axis.axis),
            flexes: axis.flexes.lock().clone(),
            members: axis
                .members
                .iter()
                .map(|member| arrangement_of(member, panes))
                .collect(),
        },
    }
}

/// What one pane holds, as far as a layout can say.
///
/// `None` for a pane with no nameable tab at all: an axis member that collapses
/// rather than a leaf with an empty tab list, because an empty leaf would be
/// rendered back as an empty pane.
fn leaf_of_pane(
    pane: &Entity<Pane>,
    active_pane: &Entity<Pane>,
    zed_workspace: &Workspace,
    cx: &App,
) -> Option<Leaf> {
    let read = pane.read(cx);
    let mut tabs = Vec::new();
    let mut active = 0;
    for (index, item) in read.items().enumerate() {
        let Some(tab) = tab_of_item(item.as_ref(), zed_workspace, cx) else {
            continue;
        };
        // Against the kept tabs, not the pane's: an unnameable tab ahead of the
        // active one would otherwise shift the index the document restores.
        if index == read.active_item_index() {
            active = tabs.len();
        }
        tabs.push(tab);
    }
    (!tabs.is_empty()).then(|| Leaf {
        tabs,
        active,
        focused: pane == active_pane,
    })
}

/// The tab one item is, if a layout can name it.
///
/// Three cases in priority order: a terminal attached to one of our sessions, a
/// placeholder still holding the tab it stood in for, and anything with a file
/// behind it. Everything else — an untitled buffer, a diagnostics page, an
/// item from a panel — is not something a `LayoutDoc` can describe, and is
/// skipped rather than guessed at.
fn tab_of_item(item: &dyn ItemHandle, zed_workspace: &Workspace, cx: &App) -> Option<Tab> {
    if let Some(session_id) = session_of_item(item, cx) {
        return Some(Tab::Terminal {
            session_id: session_id.into(),
        });
    }
    let path_style = zed_workspace.path_style(cx);
    // A placeholder is captured back as the very tab it stood in for, and that
    // tab is spelled the way this client would have spelled it had the file
    // opened. Otherwise a document would say one thing while the tab is a
    // placeholder and another the moment the file opens, which is a write per
    // failed open for every client that has one.
    if let Some(placeholder) = item.downcast::<MissingTab>() {
        return Some(portable_layout_tab(
            placeholder.read(cx).tab().clone(),
            path_style,
        ));
    }
    let project_path = item.project_path(cx)?;
    let path = zed_workspace
        .project()
        .read(cx)
        .absolute_path(&project_path, cx)?;
    Some(Tab::Editor {
        path: portable_layout_path(&path.to_string_lossy(), path_style),
    })
}

/// One tab as the document should store it. Only an editor tab has a path to
/// spell; a terminal tab is a session id and is returned untouched.
fn portable_layout_tab(tab: Tab, path_style: PathStyle) -> Tab {
    match tab {
        Tab::Editor { path } => Tab::Editor {
            path: portable_layout_path(&path, path_style),
        },
        tab @ Tab::Terminal { .. } => tab,
    }
}

/// An editor tab's path, spelled the one way every client of this workspace
/// spells it.
///
/// **The style is the project's, not this client's.** A Windows client editing
/// a Linux project over SSH holds Unix paths, where a backslash is an ordinary
/// character in a file name — `/repos/zed/we\ird.rs` is a real file, and
/// rewriting its separator would name a different one, or none. A Windows
/// project's paths are the ones that need it: Windows accepts `\` and `/`
/// interchangeably, so the same file reaches a document under two spellings,
/// and two spellings of one tab is a document that churns as clients disagree.
/// Forward slashes are the single spelling, because they are the one both
/// styles read.
fn portable_layout_path(path: &str, path_style: PathStyle) -> String {
    match path_style {
        PathStyle::Windows => path.replace('\\', "/"),
        PathStyle::Unix => path.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Keeping the two in step
// ---------------------------------------------------------------------------

/// What a pushed layout means for a window already showing one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Broadcast {
    /// Not ours, or ours already — nothing to do.
    Ignore,
    /// Somebody else moved the furniture: rebuild from the new document.
    Rerender,
}

/// Whether a broadcast layout is news.
///
/// A client sees its own accepted writes come back (see the module docs), and
/// re-rendering on one would tear down the panes the user is looking at to
/// build the same ones again. Revision is what tells them apart: anything at or
/// below what this window already has is either its own echo or a straggler
/// from before a newer write.
pub fn broadcast_action(workspace_id: &str, rev: u64, event: &LayoutEvent) -> Broadcast {
    if event.workspace_id != workspace_id || event.rev <= rev {
        return Broadcast::Ignore;
    }
    Broadcast::Rerender
}

fn same_layout_structure(left: &LayoutDoc, right: &LayoutDoc) -> bool {
    let (left_arrangement, left_leaves) = arrangement_from_layout(&left.root);
    let (right_arrangement, right_leaves) = arrangement_from_layout(&right.root);
    left.schema_version == right.schema_version
        && left_arrangement == right_arrangement
        && left_leaves
            .iter()
            .map(|leaf| &leaf.tabs)
            .eq(right_leaves.iter().map(|leaf| &leaf.tabs))
}

fn same_layout_except_focus(left: &LayoutDoc, right: &LayoutDoc) -> bool {
    if !same_layout_structure(left, right) {
        return false;
    }
    let (_, left_leaves) = arrangement_from_layout(&left.root);
    let (_, right_leaves) = arrangement_from_layout(&right.root);
    left_leaves
        .iter()
        .map(|leaf| leaf.active)
        .eq(right_leaves.iter().map(|leaf| leaf.active))
}

/// Keeps one window's pane tree and one daemon workspace's layout in step.
///
/// Held by the [`AdeLayouts`] global for as long as the window lives; dropping
/// it stops the syncing and nothing else — closing a window detaches, and kills
/// nothing.
pub struct LayoutSync {
    zed_workspace: WeakEntity<Workspace>,
    /// The window the pane tree is in, so a re-render triggered by a broadcast
    /// or a lost race has one to build into.
    window: AnyWindowHandle,
    ade_workspace: AdeWorkspace,
    lifecycle: Arc<WorkspaceLifecycleService>,
    /// The revision this window's picture is of, including a write currently
    /// awaiting its control reply. A refused write rolls this back before the
    /// stored layout is fetched.
    rev: u64,
    /// The document claimed at `rev`, so an event that changes nothing
    /// nameable costs no round trip.
    known: LayoutDoc,
    /// The session behind each terminal item, so a closed tab can be killed
    /// after the item is already gone — `ItemRemoved` carries an id and nothing
    /// else.
    sessions_by_item: HashMap<EntityId, String>,
    /// The debounce in flight, if any. Replacing it is what re-arms it.
    pending: Option<Task<()>>,
    rendering: bool,
    queued: Option<(LayoutDoc, u64)>,
    queued_is_reset: bool,
    _subscriptions: Vec<Subscription>,
}

impl LayoutSync {
    /// Starts syncing `zed_workspace` against the workspace it is showing, from
    /// the revision it was rendered at.
    pub fn new(
        zed_workspace: &Entity<Workspace>,
        ade_workspace: AdeWorkspace,
        layout: LayoutDoc,
        rev: u64,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let lifecycle = crate::lifecycle_service(cx);
        let weak = zed_workspace.downgrade();
        let window_handle = window.window_handle();
        cx.new(|cx| {
            let subscriptions = vec![
                // Everything that reshapes the centre notifies the workspace —
                // splits, tab moves, closes, activation. Observing rather than
                // matching events is deliberate: the capture is compared
                // against what is already stored, so an event that changes no
                // tab is free, and one this crate has not thought of is still
                // caught.
                cx.observe_in(zed_workspace, window, |this: &mut Self, _, _, cx| {
                    this.schedule(cx)
                }),
                cx.subscribe(zed_workspace, |this: &mut Self, _, event, cx| {
                    this.on_workspace_event(event, cx)
                }),
            ];
            Self {
                zed_workspace: weak,
                window: window_handle,
                ade_workspace,
                lifecycle,
                rev,
                known: layout,
                sessions_by_item: HashMap::new(),
                pending: None,
                rendering: false,
                queued: None,
                queued_is_reset: false,
                _subscriptions: subscriptions,
            }
        })
    }

    /// The revision this window is showing.
    pub fn rev(&self) -> u64 {
        self.rev
    }

    /// The workspace this window is syncing, as the daemon names it.
    pub fn daemon_workspace_id(&self) -> String {
        self.ade_workspace.daemon_workspace_id()
    }

    /// Re-arms the debounce. Called on every mutation; only the last one in a
    /// burst reaches [`Self::push`].
    fn schedule(&mut self, cx: &mut Context<Self>) {
        if self.rendering {
            return;
        }
        self.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PUSH_DEBOUNCE).await;
            this.update(cx, |this, cx| this.push(cx)).log_err();
        }));
    }

    /// Captures the window and stores it, if it says anything new.
    fn push(&mut self, cx: &mut Context<Self>) {
        let Some(layout) = self
            .zed_workspace
            .read_with(cx, |zed_workspace, cx| {
                capture_persistable_layout(zed_workspace, cx)
            })
            .log_err()
            .flatten()
        else {
            return;
        };
        self.remember_sessions(cx);
        if layout == self.known {
            return;
        }

        let rev = self.rev + 1;
        let previous_rev = self.rev;
        let previous_known = std::mem::replace(&mut self.known, layout.clone());
        self.rev = rev;
        let lifecycle = self.lifecycle.clone();
        let ade_workspace = self.ade_workspace.clone();
        let stored = layout;
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn({
                    let stored = stored.clone();
                    async move { lifecycle.update_layout(&ade_workspace, &stored, rev) }
                })
                .await;
            if let Err(error) = outcome {
                this.update(cx, |this, cx| {
                    if this.rev == rev && this.known == stored {
                        this.rev = previous_rev;
                        this.known = previous_known;
                    }
                    log::warn!(
                        "layout rev {rev} for {} was refused, re-reading: {error:#}",
                        this.ade_workspace.id
                    );
                    this.refetch(cx);
                })
                .log_err();
            }
        })
        .detach();
    }

    /// Re-reads the stored layout and rebuilds the window from it — **but only
    /// if the stored revision has actually moved past this window's.**
    ///
    /// A write is refused for two quite different reasons, and the caller
    /// cannot tell them apart from the error alone. Either somebody else got
    /// there first, and their arrangement is the one to show; or the document
    /// itself was rejected — the daemon refuses one naming a session it does
    /// not have, which is the standing state of a window holding a
    /// [`MissingTab`] placeholder for a session that has since died. In that
    /// second case the stored revision has not moved, the document that comes
    /// back is *older* than what is on screen, and rendering it would tear the
    /// centre down and rebuild it without every tab the user has opened since.
    ///
    /// **Unrepresentable is not closeable.** A tab this client cannot get
    /// stored is still the user's tab; a refused write is the daemon's problem
    /// to report, not a licence to close it. So a revision that has not moved
    /// leaves the window exactly as it is, and the next mutation tries the push
    /// again.
    fn refetch(&mut self, cx: &mut Context<Self>) {
        let lifecycle = self.lifecycle.clone();
        let ade_workspace = self.ade_workspace.clone();
        cx.spawn(async move |this, cx| {
            let stored = cx
                .background_spawn({
                    let ade_workspace = ade_workspace.clone();
                    async move { lifecycle.open_workspace_layout(&ade_workspace) }
                })
                .await
                .log_err()?;
            this.update(cx, |this, cx| {
                if stored.rev <= this.rev {
                    log::warn!(
                        "layout for {} was rejected rather than outraced — stored rev {} is not \
                         past this window's {}, so its tabs are left alone",
                        this.ade_workspace.id,
                        stored.rev,
                        this.rev
                    );
                    return;
                }
                this.accept(stored.layout, stored.rev, cx);
            })
            .log_err()
        })
        .detach();
    }

    /// Renders a layout the daemon says is current, and takes its revision.
    fn apply(&mut self, layout: LayoutDoc, rev: u64, cx: &mut Context<Self>) {
        let expected_current = self
            .zed_workspace
            .read_with(cx, |zed_workspace, cx| {
                capture_persistable_layout(zed_workspace, cx)
                    .map(|layout| (layout, zed_workspace.active_pane().clone()))
            })
            .ok()
            .flatten();
        self.rev = rev;
        self.known = layout.clone();
        // The rebuild is a mutation like any other, and would otherwise be
        // captured and pushed straight back.
        self.pending = None;
        self.rendering = true;
        let Some(zed_workspace) = self.zed_workspace.upgrade() else {
            self.rendering = false;
            return;
        };
        let ade_workspace = self.ade_workspace.clone();
        // The window this sync was installed in. A re-render needs one, and
        // neither a broadcast nor a refused write arrives holding it.
        let window = self.window;
        cx.spawn(async move |this, cx| {
            let render = window.update(cx, |_, window, cx| {
                render_layout_if_unchanged(
                    &zed_workspace,
                    &ade_workspace,
                    layout,
                    expected_current,
                    window,
                    cx,
                )
            });
            if let Ok(render) = render {
                render.await.log_err();
            }
            this.update(cx, |this, cx| {
                this.rendering = false;
                this.remember_sessions(cx);
                if let Some((layout, rev)) = this.queued.take() {
                    if std::mem::take(&mut this.queued_is_reset) {
                        this.apply(layout, rev, cx);
                    } else {
                        this.on_layout_event(
                            &LayoutEvent {
                                workspace_id: this.ade_workspace.daemon_workspace_id(),
                                layout,
                                rev,
                            },
                            cx,
                        );
                    }
                }
                this.schedule(cx);
            })
            .log_err();
        })
        .detach();
    }

    fn accept(&mut self, layout: LayoutDoc, rev: u64, cx: &mut Context<Self>) {
        if rev <= self.rev {
            return;
        }
        if self.rendering {
            if self
                .queued
                .as_ref()
                .is_none_or(|(_, queued_rev)| rev > *queued_rev)
            {
                self.queued = Some((layout, rev));
                self.queued_is_reset = false;
            }
            return;
        }
        if same_layout_structure(&layout, &self.known) {
            self.rev = rev;
            self.known = self
                .zed_workspace
                .read_with(cx, |zed_workspace, cx| {
                    capture_persistable_layout(zed_workspace, cx)
                })
                .ok()
                .flatten()
                .unwrap_or(layout);
            return;
        }
        self.apply(layout, rev, cx);
    }

    /// Applies one broadcast layout, if it is news. See [`broadcast_action`].
    pub fn on_layout_event(&mut self, event: &LayoutEvent, cx: &mut Context<Self>) {
        let workspace_id = self.ade_workspace.daemon_workspace_id();
        if broadcast_action(&workspace_id, self.rev, event) == Broadcast::Ignore {
            return;
        }
        if !self.rendering
            && let Some(local) = self
                .zed_workspace
                .read_with(cx, |zed_workspace, cx| {
                    capture_persistable_layout(zed_workspace, cx)
                })
                .log_err()
                .flatten()
            && local != self.known
        {
            self.rev = event.rev;
            self.known = event.layout.clone();
            self.schedule(cx);
            return;
        }
        self.accept(event.layout.clone(), event.rev, cx);
    }

    /// Takes an authoritative daemon incarnation even when its revision moved
    /// backwards. The window still owns the same workspace; only its revision
    /// history was replaced while the event stream was disconnected.
    fn on_workspace_reset(&mut self, event: &LayoutEvent, cx: &mut Context<Self>) {
        if event.workspace_id != self.ade_workspace.daemon_workspace_id() {
            return;
        }
        self.pending = None;
        self.rev = event.rev;
        self.known = event.layout.clone();
        if self.rendering {
            self.queued = Some((event.layout.clone(), event.rev));
            self.queued_is_reset = true;
        } else {
            self.apply(event.layout.clone(), event.rev, cx);
        }
    }

    /// Keeps the item → session map current, so a tab that is closed can still
    /// be identified after its item is gone.
    fn remember_sessions(&mut self, cx: &mut Context<Self>) {
        let Ok(sessions) = self.zed_workspace.read_with(cx, |zed_workspace, cx| {
            let mut sessions = HashMap::new();
            for pane in zed_workspace.panes() {
                for item in pane.read(cx).items() {
                    if let Some(session) = session_of_item(item.as_ref(), cx) {
                        sessions.insert(item.item_id(), session);
                    }
                }
            }
            sessions
        }) else {
            return;
        };
        self.sessions_by_item = sessions;
    }

    /// **Closing a terminal tab kills its session** (operator ruling,
    /// 2026-08-04): the tab is the only handle on it, so a tab closed and a
    /// session left running would be a process nothing can reach.
    ///
    /// **Moving one does not.** Zed implements a tab dragged into another pane —
    /// or dropped past an edge to make a split — as a remove from the old pane
    /// followed by an add to the new one ([`workspace::move_item`]), and that
    /// remove emits the very same `ItemRemoved` a close does. Nothing on the
    /// event tells them apart, and Zed has no move-shaped event to subscribe to
    /// instead, so the question is asked of the window rather than the event:
    /// one turn later, an item still standing somewhere means the remove was
    /// half of a move, and only its absence is a close.
    ///
    /// Closing the *window* is not this either — no item is removed, and a
    /// question asked of a workspace that has already gone answers "still
    /// standing", so teardown kills nothing.
    fn on_workspace_event(&mut self, event: &workspace::Event, cx: &mut Context<Self>) {
        let item_id = match event {
            workspace::Event::ItemAdded { item } => {
                if let Some(session) = session_of_item(item.as_ref(), cx) {
                    self.sessions_by_item.insert(item.item_id(), session);
                }
                return;
            }
            workspace::Event::ItemRemoved { item_id } => *item_id,
            _ => return,
        };
        // Left in the map until the decision is made: after a move the item is
        // the same item, and its mapping is still the truth.
        let Some(session) = self.sessions_by_item.get(&item_id).cloned() else {
            return;
        };
        // Not `cx.defer`: a deferred callback runs inside the same effect flush
        // as the emit that queued it, which is not guaranteed to be after the
        // add half of a move. A spawned foreground task runs once the update
        // that removed the item has settled, whatever it went on to do.
        cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| {
                if this.still_open(item_id, &session, cx) {
                    return;
                }
                this.sessions_by_item.remove(&item_id);
                let lifecycle = this.lifecycle.clone();
                let ade_workspace = this.ade_workspace.clone();
                cx.background_spawn(async move {
                    lifecycle.kill_session(&ade_workspace, &session).log_err();
                })
                .detach();
            })
            .log_err();
        })
        .detach();
        self.schedule(cx);
    }

    /// Whether the removed item is still somewhere in this window: under its own
    /// id, which is what a move preserves, or as a different item carrying the
    /// same session, which is what a re-render leaves behind.
    ///
    /// A window that is already gone answers yes. Detaching must not kill, and
    /// "nothing left to look in" is not evidence of a close.
    fn still_open(&self, item_id: EntityId, session: &str, cx: &App) -> bool {
        self.zed_workspace
            .read_with(cx, |zed_workspace, cx| {
                zed_workspace.panes().iter().any(|pane| {
                    pane.read(cx).items().any(|item| {
                        item.item_id() == item_id
                            || session_of_item(item.as_ref(), cx).as_deref() == Some(session)
                            || item.downcast::<MissingTab>().is_some_and(|placeholder| {
                                matches!(
                                    placeholder.read(cx).tab(),
                                    Tab::Terminal { session_id } if session_id.as_str() == session
                                )
                            })
                    })
                })
            })
            .unwrap_or(true)
    }
}

/// Every window currently syncing a layout, so a broadcast can be routed to the
/// one showing that workspace.
#[derive(Default)]
pub struct AdeLayouts {
    syncs: HashMap<EntityId, Entity<LayoutSync>>,
    /// The one reader of the merged layout stream, started with the first sync.
    _stream: Option<Task<()>>,
}

impl Global for AdeLayouts {}

impl AdeLayouts {
    /// Starts (or replaces) the sync for one window.
    ///
    /// **Call this with `zed_workspace` un-leased** — from `cx.update`, not
    /// from inside a `zed_workspace.update`/`update_in` closure. It reads the
    /// workspace's panes below to seed the item → session map, and a read
    /// nested in an update is the double lease GPUI panics on. That panic is
    /// fatal rather than merely loud: on Windows it unwinds into a nounwind
    /// window procedure and aborts the process.
    pub fn install(
        zed_workspace: &Entity<Workspace>,
        ade_workspace: AdeWorkspace,
        layout: LayoutDoc,
        rev: u64,
        window: &mut Window,
        cx: &mut App,
    ) {
        let sync = LayoutSync::new(zed_workspace, ade_workspace, layout, rev, window, cx);
        let id = zed_workspace.entity_id();
        // The window going away is what ends its sync — held here rather than
        // by the window, since a global is what the layout stream can reach.
        // Dropping the sync detaches and nothing else; the sessions carry on.
        let release = cx.observe_release(zed_workspace, move |_, cx| {
            cx.default_global::<Self>().syncs.remove(&id);
        });
        sync.update(cx, |sync, cx| {
            sync._subscriptions.push(release);
            // The map a closed tab is identified by, before any tab can be
            // closed — the debounced capture that also refreshes it may not have
            // run yet.
            sync.remember_sessions(cx);
            // **What the window shows wins over what was stored.** A window
            // built from `layout` captures back as `layout` and this costs
            // nothing ([`LayoutSync::push`] returns on an unchanged document);
            // a window that was *attached* rather than built holds a terminal
            // the stored document may not name — a recreated session standing
            // where a dead one is still written — and only a push corrects
            // that. Debounced like every other capture, so the window is
            // settled before it is read.
            sync.schedule(cx);
        });
        cx.default_global::<Self>().syncs.insert(id, sync);
        // Windows come and go; the stream is opened once and outlives them.
        if cx.default_global::<Self>()._stream.is_none() {
            let stream = Self::stream(cx);
            cx.default_global::<Self>()._stream = stream;
        }
    }

    /// Feeds every host's accepted layouts — and every killed workspace — to
    /// whichever window is showing them.
    fn stream(cx: &mut App) -> Option<Task<()>> {
        let events = crate::lifecycle_service(cx).subscribe_layout().log_err()?;
        Some(cx.spawn(async move |cx| {
            // Ends when the stream closes; a shut-down app drops this task with
            // the global that holds it, which is the other way out.
            while let Ok(event) = events.recv().await {
                cx.update(|cx| Self::handle_event(event, cx));
            }
        }))
    }

    fn handle_event(event: WorkspaceEvent, cx: &mut App) {
        match event {
            WorkspaceEvent::Layout { remote_host, event } => {
                for sync in Self::syncs_for_host(remote_host.as_deref(), cx) {
                    sync.update(cx, |sync, cx| sync.on_layout_event(&event, cx));
                }
            }
            WorkspaceEvent::Reset { remote_host, event } => {
                for sync in Self::syncs_for_host(remote_host.as_deref(), cx) {
                    sync.update(cx, |sync, cx| sync.on_workspace_reset(&event, cx));
                }
            }
            WorkspaceEvent::Removed {
                remote_host,
                workspace_id,
            } => Self::forget(remote_host.as_deref(), &workspace_id, cx),
        }
    }

    /// The sync one window is running, if it has one.
    ///
    /// `&App`, and no `default_global`: a reader must never be what brings the
    /// global up, and a window with no sync is the answer rather than a reason
    /// to make one.
    fn sync_for(zed_workspace: EntityId, cx: &App) -> Option<Entity<LayoutSync>> {
        cx.try_global::<Self>()?.syncs.get(&zed_workspace).cloned()
    }

    pub(crate) fn is_showing(
        zed_workspace: EntityId,
        ade_workspace: &AdeWorkspace,
        cx: &App,
    ) -> bool {
        Self::sync_for(zed_workspace, cx).is_some_and(|sync| {
            sync.read(cx).daemon_workspace_id() == ade_workspace.daemon_workspace_id()
        })
    }

    pub(crate) fn forget_window(zed_workspace: EntityId, cx: &mut App) {
        if cx.has_global::<Self>() {
            cx.global_mut::<Self>().syncs.remove(&zed_workspace);
        }
    }

    /// Whether `zed_workspace` is already syncing `ade_workspace` — in which
    /// case the fresh `stored` read is *offered* to its sync, which renders it
    /// only if it is news (a revision past the window's own).
    ///
    /// This is what makes re-opening a workspace that is already on screen a
    /// focus rather than a rebuild. The open path reads the stored layout
    /// unconditionally, and building it again would tear down every pane and
    /// re-attach every terminal the window is showing — a full flicker-and-
    /// repaint for a click that changed nothing.
    pub(crate) fn catch_up_if_showing(
        zed_workspace: EntityId,
        ade_workspace: &AdeWorkspace,
        stored: &WorkspaceLayout,
        cx: &mut App,
    ) -> bool {
        let Some(sync) = Self::sync_for(zed_workspace, cx) else {
            return false;
        };
        if sync.read(cx).daemon_workspace_id() != ade_workspace.daemon_workspace_id() {
            return false;
        }
        let event = LayoutEvent {
            workspace_id: ade_workspace.daemon_workspace_id(),
            layout: stored.layout.clone(),
            rev: stored.rev,
        };
        sync.update(cx, |sync, cx| {
            if stored.rev > sync.rev {
                sync.on_layout_event(&event, cx);
            }
        });
        true
    }

    fn syncs(cx: &mut App) -> Vec<Entity<LayoutSync>> {
        cx.default_global::<Self>()
            .syncs
            .values()
            .cloned()
            .collect()
    }

    fn syncs_for_host(remote_host: Option<&str>, cx: &mut App) -> Vec<Entity<LayoutSync>> {
        Self::syncs(cx)
            .into_iter()
            .filter(|sync| sync.read(cx).ade_workspace.remote_host.as_deref() == remote_host)
            .collect()
    }

    /// **A workspace somebody killed.** Every window syncing it stops, and the
    /// ledger is asked for a fresh pass so its row stops claiming a session.
    ///
    /// The window and its tabs stay open, but its ADE ownership and binding are
    /// released so its project can reconnect or create an ordinary terminal.
    /// The terminals in it are already dead — their sessions went with the
    /// workspace, so each attach client exited — and syncing must stop because
    /// a push from here would recreate the record the kill deleted.
    fn forget(remote_host: Option<&str>, workspace_id: &str, cx: &mut App) {
        let doomed = cx
            .default_global::<Self>()
            .syncs
            .iter()
            .map(|(id, sync)| (*id, sync.clone()))
            .collect::<Vec<_>>()
            .into_iter()
            .filter(|(_, sync)| {
                let sync = sync.read(cx);
                sync.ade_workspace.remote_host.as_deref() == remote_host
                    && sync.daemon_workspace_id() == workspace_id
            })
            .map(|(id, sync)| {
                let sync = sync.read(cx);
                (id, sync.zed_workspace.clone(), sync.window)
            })
            .collect::<Vec<_>>();
        for (id, zed_workspace, window) in doomed {
            cx.default_global::<Self>().syncs.remove(&id);
            crate::workspace_view::clear_window_binding(id, cx);
            crate::connect::release_window_claim(id, cx);
            cx.defer(move |cx| {
                if Self::sync_for(id, cx).is_some() {
                    return;
                }
                window
                    .update(cx, |_, window, cx| {
                        zed_workspace
                            .update(cx, |zed_workspace, cx| {
                                zed_workspace.clear_ade_owns_layout(window, cx);
                                zed_workspace.set_window_title_override(None, window, cx);
                            })
                            .log_err();
                    })
                    .log_err();
            });
        }
        // The row is the other view of the same fact. Only if something has
        // already brought the store up: a broadcast must not be what starts it.
        if let Some(store) = crate::AdeWorkspaceStore::try_global(cx) {
            store.update(cx, |store, cx| store.refresh(cx));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ade_session::proto::SessionId;

    fn terminal(id: &str) -> Tab {
        Tab::Terminal {
            session_id: SessionId::new(id),
        }
    }

    fn editor(path: &str) -> Tab {
        Tab::Editor {
            path: path.to_owned(),
        }
    }

    fn leaf(tabs: Vec<Tab>, active: usize, focused: bool) -> LayoutNode {
        LayoutNode::Leaf {
            tabs,
            active,
            focused,
        }
    }

    fn split(dir: SplitDir, ratio: f32, first: LayoutNode, second: LayoutNode) -> LayoutNode {
        LayoutNode::Split {
            dir,
            ratio,
            children: Box::new([first, second]),
        }
    }

    /// The identity the round-trip tests assert: unfold, then fold back with
    /// every leaf answered exactly as it came out.
    fn round_trip(root: &LayoutNode) -> Option<LayoutNode> {
        let (arrangement, leaves) = arrangement_from_layout(root);
        layout_from_arrangement(&arrangement, &mut |index| leaves.get(index).cloned())
    }

    #[test]
    fn test_a_one_leaf_document_is_one_pane_holding_its_tabs() {
        let root = leaf(vec![terminal("s1"), editor("/repos/zed/main.rs")], 1, true);
        let (arrangement, leaves) = arrangement_from_layout(&root);

        assert_eq!(arrangement, Arrangement::Pane(0));
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].tabs.len(), 2);
        assert_eq!(leaves[0].active, 1);
        assert!(leaves[0].focused);
    }

    #[test]
    fn test_an_active_index_past_the_tabs_is_clamped_not_trusted() {
        let (_, leaves) = arrangement_from_layout(&leaf(vec![terminal("s1")], 7, false));
        assert_eq!(leaves[0].active, 0);
        // And an empty leaf has no tab to be active.
        let (_, leaves) = arrangement_from_layout(&leaf(Vec::new(), 3, false));
        assert_eq!(leaves[0].active, 0);
    }

    #[test]
    fn test_a_split_becomes_an_axis_whose_flexes_carry_the_ratio() {
        let root = split(
            SplitDir::Horizontal,
            0.3,
            leaf(vec![terminal("left")], 0, false),
            leaf(vec![terminal("right")], 0, true),
        );
        let (arrangement, leaves) = arrangement_from_layout(&root);

        assert_eq!(
            arrangement,
            Arrangement::Axis {
                dir: SplitDir::Horizontal,
                flexes: vec![0.6, 1.4],
                members: vec![Arrangement::Pane(0), Arrangement::Pane(1)],
            }
        );
        // Leaves are numbered in tree order, which is what the builder fills
        // its panes in.
        assert_eq!(leaves.len(), 2);
        assert!(!leaves[0].focused);
        assert!(leaves[1].focused);
    }

    #[test]
    fn test_flexes_always_sum_to_the_member_count() {
        for ratio in [0.0, 0.1, 0.5, 0.9, 1.0, f32::NAN] {
            let flexes = split_flexes(ratio);
            assert!(
                (flexes.iter().sum::<f32>() - 2.0).abs() < 1e-5,
                "{ratio} gave {flexes:?}"
            );
        }
    }

    #[test]
    fn test_a_degenerate_ratio_is_clamped_to_something_draggable() {
        let narrow = 2.0 * MIN_SPLIT_RATIO;
        let close = |flexes: Vec<f32>, expected: [f32; 2]| {
            assert!(
                flexes
                    .iter()
                    .zip(expected)
                    .all(|(flex, expected)| (flex - expected).abs() < 1e-5),
                "{flexes:?} is not {expected:?}"
            );
        };
        close(split_flexes(0.0), [narrow, 2.0 - narrow]);
        close(split_flexes(1.0), [2.0 - narrow, narrow]);
        // Not a number at all is a half rather than a panic.
        assert_eq!(split_flexes(f32::NAN), vec![1.0, 1.0]);
    }

    #[test]
    fn test_nested_splits_round_trip_unchanged() {
        let root = split(
            SplitDir::Horizontal,
            0.25,
            leaf(vec![terminal("a")], 0, false),
            split(
                SplitDir::Vertical,
                0.5,
                leaf(vec![editor("/repos/zed/b.rs")], 0, true),
                leaf(vec![terminal("c"), editor("/repos/zed/d.rs")], 1, false),
            ),
        );
        assert_eq!(round_trip(&root).as_ref(), Some(&root));
    }

    /// A backslash is an ordinary character in a Unix file name, so a Unix
    /// project's paths are stored exactly as they are. Rewriting one would name
    /// a different file, or none — and this client's own platform has no say in
    /// it, which is the whole point of asking the project.
    #[test]
    fn test_a_unix_project_keeps_the_backslashes_in_its_paths() {
        for path in [
            r"/repos/zed/we\ird.rs",
            r"/repos/zed/a\b\c.rs",
            r"/repos/zed/trailing\",
            "/repos/zed/plain.rs",
        ] {
            assert_eq!(portable_layout_path(path, PathStyle::Unix), path);
        }
    }

    /// Windows reads `\` and `/` as the same separator, so one file can reach a
    /// document under two spellings and two clients would each rewrite the
    /// other's. One spelling settles it, and it is the one both styles read.
    #[test]
    fn test_a_windows_project_stores_one_separator() {
        assert_eq!(
            portable_layout_path(r"C:\repos\zed\a.rs", PathStyle::Windows),
            "C:/repos/zed/a.rs"
        );
        // Mixed on the way in, single on the way out — and already-normalized
        // input is left alone, so the capture is idempotent.
        for path in [
            r"C:\repos\zed/a.rs",
            r"C:/repos\zed\a.rs",
            "C:/repos/zed/a.rs",
        ] {
            assert_eq!(
                portable_layout_path(path, PathStyle::Windows),
                "C:/repos/zed/a.rs",
                "{path} was not normalized"
            );
        }
        assert_eq!(
            portable_layout_path(r"\\server\share\a.rs", PathStyle::Windows),
            "//server/share/a.rs"
        );
    }

    /// The placeholder standing in for a file that would not open captures back
    /// as the same tab an opened file would have — so a client that cannot open
    /// a file does not spell its tab differently from one that can. A terminal
    /// tab has no path to spell and is untouched, under either style.
    #[test]
    fn test_a_placeholder_tab_is_spelled_like_an_opened_one() {
        assert_eq!(
            portable_layout_tab(editor(r"C:\repos\zed\gone.rs"), PathStyle::Windows),
            editor("C:/repos/zed/gone.rs")
        );
        assert_eq!(
            portable_layout_tab(editor(r"/repos/zed/we\ird.rs"), PathStyle::Unix),
            editor(r"/repos/zed/we\ird.rs")
        );
        for path_style in [PathStyle::Windows, PathStyle::Unix] {
            assert_eq!(
                portable_layout_tab(terminal(r"session\one"), path_style),
                terminal(r"session\one"),
                "a terminal tab is a session id, not a path"
            );
        }
    }

    #[test]
    fn test_an_n_ary_axis_folds_into_binary_splits_of_the_same_picture() {
        // What Zed's own tree looks like after two splits the same way: one
        // axis, three members, equal flexes.
        let arrangement = Arrangement::Axis {
            dir: SplitDir::Horizontal,
            flexes: vec![1.0, 1.0, 1.0],
            members: vec![
                Arrangement::Pane(0),
                Arrangement::Pane(1),
                Arrangement::Pane(2),
            ],
        };
        let leaves: Vec<Leaf> = ["a", "b", "c"]
            .iter()
            .map(|id| Leaf {
                tabs: vec![terminal(id)],
                active: 0,
                focused: false,
            })
            .collect();

        let root =
            layout_from_arrangement(&arrangement, &mut |index| leaves.get(index).cloned()).unwrap();

        let LayoutNode::Split {
            ratio, children, ..
        } = &root
        else {
            panic!("a three-member axis must fold into a split, got {root:?}");
        };
        // The first pane's third, and the other two halving what is left.
        assert!((ratio - 1.0 / 3.0).abs() < 1e-5, "outer ratio was {ratio}");
        let LayoutNode::Split { ratio, .. } = &children[1] else {
            panic!("the remainder must itself be a split");
        };
        assert!((ratio - 0.5).abs() < 1e-5, "inner ratio was {ratio}");
        // And rebuilding it gives back the sizes it started with.
        let (rebuilt, _) = arrangement_from_layout(&root);
        let Arrangement::Axis { flexes, .. } = &rebuilt else {
            panic!("expected an axis");
        };
        assert!((flexes[0] - 2.0 / 3.0).abs() < 1e-5, "{flexes:?}");
    }

    #[test]
    fn test_a_pane_holding_nothing_nameable_collapses_its_split() {
        let root = split(
            SplitDir::Vertical,
            0.5,
            leaf(vec![terminal("kept")], 0, true),
            leaf(vec![terminal("gone")], 0, false),
        );
        let (arrangement, leaves) = arrangement_from_layout(&root);

        // The second pane came out empty: the split collapses into the first
        // rather than leaving a hole.
        let folded = layout_from_arrangement(&arrangement, &mut |index| {
            (index == 0).then(|| leaves[0].clone())
        });
        assert_eq!(folded, Some(leaf(vec![terminal("kept")], 0, true)));

        // Every pane empty means there is nothing to store at all.
        assert_eq!(
            layout_from_arrangement(&arrangement, &mut |_| None),
            None,
            "an empty window must not overwrite a real document"
        );
    }

    /// The stamp and the reader must agree, because they live in different
    /// crates. `ade_workspaces` writes the prefix onto a session terminal's
    /// task id; `terminal_view` reads it back to keep that terminal out of the
    /// save prompts a running *task* belongs in. A second spelling of the
    /// prefix would be a second answer to "is this a session terminal?", and
    /// the symptom would be the close dialog coming back.
    #[test]
    fn test_a_session_terminal_is_recognisable_as_one_from_outside_this_crate() {
        let stamped = session_task_id("1ca7ed0b");
        assert!(
            stamped.is_ade_session(),
            "terminal_view must recognise what this crate stamps: {stamped:?}"
        );
        assert_eq!(
            stamped.0.strip_prefix(SESSION_TASK_PREFIX),
            Some("1ca7ed0b"),
            "and the session id must still be readable back off it"
        );

        // And a task the user actually asked to run is left alone, so closing
        // a window with one still asks about it.
        assert!(!TaskId("cargo test".to_owned()).is_ade_session());
    }

    #[test]
    fn test_the_direction_survives_both_translations() {
        for dir in [SplitDir::Horizontal, SplitDir::Vertical] {
            assert_eq!(dir_of(axis_of(dir)), dir);
        }
        assert_eq!(axis_of(SplitDir::Horizontal), Axis::Horizontal);
        assert_eq!(axis_of(SplitDir::Vertical), Axis::Vertical);
    }

    #[test]
    fn test_pruned_flexes_keep_the_survivors_relative_sizes() {
        // Two of three members left, at 1:3 — rescaled to sum to two.
        let flexes = normalized(vec![0.5, 1.5]);
        assert!(
            (flexes.iter().sum::<f32>() - 2.0).abs() < 1e-5,
            "{flexes:?}"
        );
        assert!((flexes[1] / flexes[0] - 3.0).abs() < 1e-5, "{flexes:?}");
        // Nothing to rescale against is an even split rather than a division
        // by zero.
        assert_eq!(normalized(vec![0.0, 0.0]), vec![1.0, 1.0]);
    }

    #[test]
    fn test_a_terminals_session_is_read_back_off_its_task_id() {
        assert_eq!(session_task_id("abc"), TaskId("ade-session:abc".into()));
        // The prefix is what tells one of our terminals from a user's task
        // terminal, which must never be captured as a session tab.
        assert!(
            !TaskId("cargo test".into())
                .0
                .starts_with(SESSION_TASK_PREFIX)
        );
    }

    // -----------------------------------------------------------------
    // Against a real window
    //
    // Editor tabs and *unattachable* terminal tabs, deliberately: an attached
    // terminal would need a session daemon and a pty, which `daemon_backend`'s
    // tests cover on their own side of the seam. What is under test here is
    // the pane tree — splits, ratios, tab order, which tab is active, which
    // pane has focus — and that is the same tree whatever the tabs hold.
    // -----------------------------------------------------------------

    use crate::{
        DaemonEvent, GlobalLifecycleService, SessionBackend, SessionId as SeamSessionId,
        SessionInfo, SessionSpec, StatusDelivery, WorkspaceLayout,
    };
    use anyhow::{Context as _, bail};
    use gpui::{TestAppContext, VisualTestContext};
    use std::sync::Mutex;
    use terminal_view::TerminalId;
    use workspace::{
        SaveIntent,
        item::test::{TestItem, TestProjectItem},
    };

    /// A backend whose sessions are all somewhere this client cannot reach.
    ///
    /// The one way a *live* session fails to attach, and so the only way to
    /// exercise the tab that stands in for it. It also records every call, so a
    /// test can assert the thing that matters most here: that nothing killed.
    #[derive(Default)]
    struct UnreachableBackend {
        calls: Mutex<Vec<String>>,
        created_working_directories: Mutex<Vec<PathBuf>>,
        /// The stored layout per workspace, for the tests that need the write
        /// side of the seam to behave like the daemon's. Empty by default, and
        /// a workspace that was never seeded is "no such workspace" — so a test
        /// that does not ask for a store keeps the old behaviour, where every
        /// push is refused and every re-read fails.
        layouts: Mutex<HashMap<String, WorkspaceLayout>>,
        /// The sessions this backend admits to having. A `Terminal` tab naming
        /// anything else is refused exactly as the daemon refuses it, which is
        /// the state a pane holding a placeholder for a dead session leaves a
        /// window in.
        live_sessions: Mutex<Vec<String>>,
    }

    impl UnreachableBackend {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn killed_anything(&self) -> bool {
            self.calls().iter().any(|call| call.contains("kill"))
        }

        fn created_working_directories(&self) -> Vec<PathBuf> {
            self.created_working_directories.lock().unwrap().clone()
        }

        /// Gives the backend a stored layout to answer reads with, and to guard
        /// writes against.
        fn seed_layout(&self, workspace_id: &str, layout: LayoutDoc, rev: u64) {
            self.layouts
                .lock()
                .unwrap()
                .insert(workspace_id.to_owned(), WorkspaceLayout { layout, rev });
        }

        /// Stands in for another client writing to the same workspace.
        fn write_behind_the_clients_back(&self, workspace_id: &str, layout: LayoutDoc, rev: u64) {
            self.seed_layout(workspace_id, layout, rev);
        }

        fn stored_rev(&self, workspace_id: &str) -> Option<u64> {
            self.layouts
                .lock()
                .unwrap()
                .get(workspace_id)
                .map(|stored| stored.rev)
        }
    }

    impl SessionBackend for UnreachableBackend {
        fn create(&self, spec: &SessionSpec) -> anyhow::Result<SeamSessionId> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("create:{}", spec.id));
            bail!("no route to the host");
        }

        fn create_session_in_workspace(
            &self,
            workspace_id: &str,
            cwd: &std::path::Path,
        ) -> anyhow::Result<String> {
            self.created_working_directories
                .lock()
                .unwrap()
                .push(cwd.to_path_buf());
            self.calls
                .lock()
                .unwrap()
                .push(format!("create_in:{workspace_id}"));
            bail!("no route to the host");
        }

        fn list(&self) -> anyhow::Result<Vec<SessionInfo>> {
            Ok(Vec::new())
        }

        fn exists(&self, _id: &SeamSessionId) -> anyhow::Result<bool> {
            Ok(false)
        }

        fn attach(&self, spec: &SessionSpec) -> anyhow::Result<crate::Attached> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("attach:{}", spec.id));
            bail!("no route to the host");
        }

        fn attach_session(&self, session_id: &str) -> anyhow::Result<Vec<String>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("attach_session:{session_id}"));
            bail!("no route to the host holding {session_id}");
        }

        fn detach(&self, _id: &SeamSessionId) -> anyhow::Result<()> {
            Ok(())
        }

        fn kill(&self, id: &SeamSessionId) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(format!("kill:{id}"));
            Ok(())
        }

        fn kill_session(&self, session_id: &str) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("kill_session:{session_id}"));
            Ok(())
        }

        fn kill_workspace(&self, workspace_id: &str) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("kill_workspace:{workspace_id}"));
            Ok(())
        }

        fn status_delivery(&self) -> StatusDelivery {
            StatusDelivery::Push
        }

        fn open_workspace(&self, workspace_id: &str) -> anyhow::Result<WorkspaceLayout> {
            self.layouts
                .lock()
                .unwrap()
                .get(workspace_id)
                .cloned()
                .with_context(|| format!("no such workspace {workspace_id}"))
        }

        /// The daemon's two rules, and only those: a revision must beat the
        /// stored one, and every terminal tab must name a session the backend
        /// has. See `ade_session_daemon::sessions::Table::update_layout`.
        fn update_layout(
            &self,
            workspace_id: &str,
            layout: &LayoutDoc,
            rev: u64,
        ) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("update_layout:{workspace_id}@{rev}"));
            let mut layouts = self.layouts.lock().unwrap();
            let stored = layouts
                .get_mut(workspace_id)
                .with_context(|| format!("no such workspace {workspace_id}"))?;
            if rev <= stored.rev {
                bail!(
                    "stale layout rev {rev} for workspace {workspace_id}, which is at {}",
                    stored.rev
                );
            }
            let live = self.live_sessions.lock().unwrap();
            for session in layout.terminal_sessions() {
                if !live.contains(&session.0) {
                    bail!("layout names unknown session {}", session.0);
                }
            }
            stored.layout = layout.clone();
            stored.rev = rev;
            Ok(())
        }

        /// A stream that never pushes: gpui's test scheduler refuses to be woken
        /// from the forwarding thread, so what a broadcast *does* is asserted by
        /// calling [`AdeLayouts::forget`] directly, and that a removal reaches
        /// the stream at all is `lifecycle`'s own fanout test.
        fn subscribe_events(&self) -> anyhow::Result<smol::channel::Receiver<DaemonEvent>> {
            let (_sender, receiver) = smol::channel::unbounded();
            Ok(receiver)
        }
    }

    /// Installs that backend behind the process-wide lifecycle service, which
    /// is where both [`render_layout`] and [`LayoutSync`] take theirs from.
    async fn install_lifecycle(
        name: &'static str,
        cx: &mut VisualTestContext,
    ) -> Arc<UnreachableBackend> {
        let backend = Arc::new(UnreachableBackend::default());
        let registry = crate::AdeWorkspaceRegistry::open_test_db(name).await;
        let service = Arc::new(WorkspaceLifecycleService::with_backend(
            registry,
            backend.clone(),
        ));
        cx.update(|_, cx| cx.set_global(GlobalLifecycleService(service)));
        backend
    }

    const ROOT: &str = "/repos/zed";

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            // Before any workspace exists, exactly as `zed::init` does it —
            // the action listeners are installed per workspace on creation.
            super::init(cx);
        });
    }

    /// A three-file project on a fake filesystem, with the globals a window
    /// needs already installed.
    async fn test_project(cx: &mut TestAppContext) -> Entity<project::Project> {
        init_test(cx);
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            ROOT,
            serde_json::json!({ "a.rs": "fn a() {}\n", "b.rs": "fn b() {}\n", "c.rs": "fn c() {}\n" }),
        )
        .await;
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        project::Project::test(fs, [ROOT.as_ref()], cx).await
    }

    /// A window over that project, and the ADE workspace it stands for.
    async fn test_window(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, AdeWorkspace, VisualTestContext) {
        let project = test_project(cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let ade_workspace = AdeWorkspace::new("Vector DB spike", "zed", ROOT);
        (workspace, ade_workspace, cx.clone())
    }

    /// The same window inside the [`MultiWorkspace`] that renders it.
    ///
    /// **Only this one can be sent an action.** A workspace's
    /// `register_action` handlers reach the dispatch tree through
    /// `Workspace::actions`, which the multi-workspace root element calls — a
    /// bare `Workspace` view renders no listeners at all, so a dispatch into
    /// one goes nowhere.
    async fn test_action_window(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, AdeWorkspace, VisualTestContext) {
        let project = test_project(cx).await;
        let (multi_workspace, cx) = cx
            .add_window_view(|window, cx| workspace::MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        let ade_workspace = AdeWorkspace::new("Vector DB spike", "zed", ROOT);
        (workspace, ade_workspace, cx.clone())
    }

    async fn render(
        workspace: &Entity<Workspace>,
        ade_workspace: &AdeWorkspace,
        layout: LayoutDoc,
        cx: &mut VisualTestContext,
    ) {
        let render =
            cx.update(|window, cx| render_layout(workspace, ade_workspace, layout, window, cx));
        render.await.unwrap();
        cx.run_until_parked();
    }

    fn path(name: &str) -> String {
        format!("{ROOT}/{name}")
    }

    #[gpui::test]
    async fn test_a_document_becomes_the_pane_tree_it_describes(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let root = split(
            SplitDir::Vertical,
            0.25,
            leaf(vec![editor(&path("a.rs"))], 0, false),
            leaf(vec![editor(&path("b.rs")), editor(&path("c.rs"))], 1, true),
        );
        render(
            &workspace,
            &ade_workspace,
            LayoutDoc::new(root.clone()),
            &mut cx,
        )
        .await;

        workspace.update(&mut cx, |workspace, cx| {
            assert_eq!(workspace.panes().len(), 2, "one pane per leaf");

            let Member::Axis(axis) = &workspace.center().root else {
                panic!("a split document must build an axis");
            };
            assert_eq!(axis.axis, Axis::Vertical, "the direction is the document's");
            assert_eq!(axis.members.len(), 2);
            let flexes = axis.flexes.lock().clone();
            assert!(
                (flexes[0] - 0.5).abs() < 1e-5,
                "a 0.25 ratio is a 0.5 flex of two: {flexes:?}"
            );

            let Member::Pane(second_pane) = &axis.members[1] else {
                panic!("the second member is one pane");
            };
            let second = second_pane.read(cx);
            assert_eq!(second.items_len(), 2, "both tabs of the leaf");
            assert_eq!(
                second.active_item_index(),
                1,
                "the document's active tab is on top"
            );
            assert_eq!(
                workspace.active_pane().entity_id(),
                second_pane.entity_id(),
                "the focused leaf is the active pane"
            );
        });
    }

    #[gpui::test]
    async fn test_the_window_captures_back_as_the_document_it_was_built_from(
        cx: &mut TestAppContext,
    ) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let root = split(
            SplitDir::Horizontal,
            0.4,
            leaf(vec![editor(&path("a.rs"))], 0, true),
            split(
                SplitDir::Vertical,
                0.5,
                leaf(vec![editor(&path("b.rs"))], 0, false),
                leaf(vec![editor(&path("c.rs"))], 0, false),
            ),
        );
        let document = LayoutDoc::new(root);
        render(&workspace, &ade_workspace, document.clone(), &mut cx).await;

        let captured = workspace
            .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
            .expect("a window showing files has a layout");
        assert_eq!(captured, document);
    }

    #[gpui::test]
    async fn test_a_file_that_cannot_be_opened_keeps_its_tab(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        // Three paths, two of which are not files. A path *inside* the project
        // is Zed's own business — it opens as an empty buffer, exactly as it
        // would if the user typed it — while a path in no worktree at all has
        // nothing to open, and gets the placeholder.
        let root = leaf(
            vec![
                editor(&path("a.rs")),
                editor(&path("deleted.rs")),
                editor("/elsewhere/gone.rs"),
            ],
            0,
            true,
        );
        let document = LayoutDoc::new(root);
        render(&workspace, &ade_workspace, document.clone(), &mut cx).await;

        workspace.update(&mut cx, |workspace, cx| {
            let pane = workspace.active_pane().read(cx);
            assert_eq!(pane.items_len(), 3, "no tab may be silently dropped");
            let placeholders: Vec<Tab> = pane
                .items()
                .filter_map(|item| item.downcast::<MissingTab>())
                .map(|item| item.read(cx).tab().clone())
                .collect();
            assert_eq!(placeholders, vec![editor("/elsewhere/gone.rs")]);
        });

        // And the document comes back whole, so one client's missing file never
        // deletes the tab for the others.
        let captured = workspace
            .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
            .expect("a window with placeholders still has a layout");
        assert_eq!(captured, document);
    }

    /// **The capture hole, closed.** A terminal tab whose session this client
    /// cannot attach to keeps its place in the document: it renders as the
    /// placeholder, captures back as the same `Tab::Terminal`, and — this is
    /// the part that matters — nothing kills the session it names.
    #[gpui::test]
    async fn test_a_session_that_will_not_attach_keeps_its_tab(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_a_session_that_will_not_attach", &mut cx).await;

        let root = leaf(
            vec![
                terminal("unreachable-session"),
                editor(&path("a.rs")),
                terminal("also-unreachable"),
            ],
            0,
            true,
        );
        let document = LayoutDoc::new(root);
        render(&workspace, &ade_workspace, document.clone(), &mut cx).await;

        workspace.update(&mut cx, |workspace, cx| {
            let pane = workspace.active_pane().read(cx);
            assert_eq!(pane.items_len(), 3, "no tab may be silently dropped");
            let placeholders: Vec<Tab> = pane
                .items()
                .filter_map(|item| item.downcast::<MissingTab>())
                .map(|item| item.read(cx).tab().clone())
                .collect();
            assert_eq!(
                placeholders,
                vec![
                    terminal("unreachable-session"),
                    terminal("also-unreachable")
                ]
            );
        });

        // The document comes back whole, so one client's unreachable host never
        // deletes a live terminal from everybody else's arrangement.
        let captured = workspace
            .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
            .expect("a window with placeholders still has a layout");
        assert_eq!(captured, document);

        // And failing to attach is not a control that says "kill".
        assert!(!backend.killed_anything(), "{:?}", backend.calls());
    }

    /// Closing a tab that is *not* a terminal is layout-only: an editor tab, or
    /// the placeholder standing in for a session that would not attach. Neither
    /// is a session this window owns, so neither may end one — the placeholder
    /// least of all, since the session behind it is running somewhere this
    /// client could not even reach.
    #[gpui::test]
    async fn test_closing_an_editor_tab_or_a_placeholder_kills_nothing(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_closing_an_editor_tab_kills_nothing", &mut cx).await;

        let document = LayoutDoc::new(leaf(
            vec![
                editor(&path("a.rs")),
                editor(&path("b.rs")),
                terminal("unreachable-session"),
            ],
            0,
            true,
        ));
        render(&workspace, &ade_workspace, document.clone(), &mut cx).await;
        cx.update(|window, cx| {
            AdeLayouts::install(
                &workspace,
                ade_workspace.clone(),
                document.clone(),
                1,
                window,
                cx,
            )
        });

        // Close them one at a time, newest first: the placeholder, then an
        // editor tab.
        for _ in 0..2 {
            let close = workspace.update_in(&mut cx, |workspace, window, cx| {
                let pane = workspace.active_pane().clone();
                let last = pane
                    .read(cx)
                    .items()
                    .last()
                    .expect("a tab to close")
                    .item_id();
                pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(last, SaveIntent::Skip, window, cx)
                })
            });
            close.await.unwrap();
            cx.run_until_parked();
        }

        assert!(
            !backend.killed_anything(),
            "closing a tab that is not a terminal must not reach the daemon: {:?}",
            backend.calls()
        );

        // Nor does closing the window: dropping the sync detaches, and that is
        // all it does.
        cx.simulate_close();
        cx.run_until_parked();
        assert!(!backend.killed_anything(), "{:?}", backend.calls());
    }

    /// Makes one already-rendered tab count as a session's, in place of the
    /// attach these tests cannot do.
    ///
    /// An *attached* terminal wants a pty and a reachable host; what makes a tab
    /// a session's as far as [`LayoutSync::on_workspace_event`] is concerned is
    /// the sync's item → session map and nothing else, so a placeholder entered
    /// into that map exercises the same decision.
    fn pretend_item_is_session(
        workspace: &Entity<Workspace>,
        item_id: EntityId,
        session_id: &str,
        cx: &mut VisualTestContext,
    ) {
        cx.update(|_, cx| {
            let sync = AdeLayouts::sync_for(workspace.entity_id(), cx)
                .expect("the window has a sync installed");
            sync.update(cx, |sync, _| {
                sync.sessions_by_item.insert(item_id, session_id.to_owned());
            });
        });
    }

    /// The operator ruling, still standing: the tab is the only handle on the
    /// session, so closing it ends the session rather than leaving a process
    /// nothing can reach.
    #[gpui::test]
    async fn test_closing_a_terminal_tab_kills_its_session(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_closing_a_terminal_tab_kills", &mut cx).await;

        let document = LayoutDoc::new(leaf(
            vec![terminal("doomed-session"), editor(&path("a.rs"))],
            0,
            true,
        ));
        render(&workspace, &ade_workspace, document.clone(), &mut cx).await;
        cx.update(|window, cx| {
            AdeLayouts::install(&workspace, ade_workspace.clone(), document, 1, window, cx)
        });

        let item_id = workspace.read_with(&mut cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .items()
                .next()
                .expect("the terminal tab")
                .item_id()
        });
        pretend_item_is_session(&workspace, item_id, "doomed-session", &mut cx);

        let close = workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.active_pane().update(cx, |pane, cx| {
                pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
            })
        });
        close.await.unwrap();
        cx.run_until_parked();

        assert!(
            backend
                .calls()
                .iter()
                .any(|call| call == "kill_session:doomed-session"),
            "closing a terminal tab must kill its session: {:?}",
            backend.calls()
        );
    }

    #[gpui::test]
    async fn test_terminal_scrub_does_not_reopen_a_file_closed_during_the_debounce(
        cx: &mut TestAppContext,
    ) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_terminal_scrub_keeps_local_closes", &mut cx).await;
        let workspace_id = ade_workspace.daemon_workspace_id();
        let stored = LayoutDoc::new(leaf(
            vec![
                editor(&path("a.rs")),
                editor(&path("b.rs")),
                terminal("doomed-session"),
            ],
            0,
            true,
        ));
        backend.seed_layout(&workspace_id, stored.clone(), 1);
        render(&workspace, &ade_workspace, stored.clone(), &mut cx).await;
        cx.update(|window, cx| {
            AdeLayouts::install(&workspace, ade_workspace.clone(), stored, 1, window, cx)
        });

        let (file, terminal) = workspace.read_with(&mut cx, |workspace, cx| {
            let items = workspace.active_pane().read(cx).items().collect::<Vec<_>>();
            (
                items.get(1).expect("the second file").item_id(),
                items.get(2).expect("the terminal").item_id(),
            )
        });
        pretend_item_is_session(&workspace, terminal, "doomed-session", &mut cx);

        for item_id in [file, terminal] {
            let close = workspace.update_in(&mut cx, |workspace, window, cx| {
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
                })
            });
            close.await.unwrap();
            cx.run_until_parked();
        }

        let scrubbed = LayoutDoc::new(leaf(
            vec![editor(&path("a.rs")), editor(&path("b.rs"))],
            0,
            true,
        ));
        backend.write_behind_the_clients_back(&workspace_id, scrubbed.clone(), 2);
        let sync = cx.update(|_, cx| {
            AdeLayouts::sync_for(workspace.entity_id(), cx).expect("the layout sync is installed")
        });
        sync.update(&mut cx, |sync, cx| {
            sync.on_layout_event(
                &LayoutEvent {
                    workspace_id: workspace_id.clone(),
                    layout: scrubbed,
                    rev: 2,
                },
                cx,
            )
        });
        cx.run_until_parked();

        let local = workspace
            .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
            .expect("the surviving file remains open");
        assert_eq!(
            local,
            LayoutDoc::new(leaf(vec![editor(&path("a.rs"))], 0, true)),
            "the daemon's terminal scrub must not restore the file closed just before it"
        );

        cx.executor().advance_clock(PUSH_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(backend.stored_rev(&workspace_id), Some(3));
        assert_eq!(
            backend
                .layouts
                .lock()
                .unwrap()
                .get(&workspace_id)
                .map(|stored| stored.layout.clone()),
            Some(local),
            "the local close should be persisted on top of the scrub revision"
        );
    }

    #[gpui::test]
    async fn test_terminal_scrub_keeps_local_closes_and_moves_across_panes(
        cx: &mut TestAppContext,
    ) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_terminal_scrub_across_panes", &mut cx).await;
        let workspace_id = ade_workspace.daemon_workspace_id();
        let initial = LayoutDoc::new(split(
            SplitDir::Horizontal,
            0.5,
            leaf(
                vec![editor(&path("a.rs")), terminal("doomed-session")],
                0,
                true,
            ),
            leaf(vec![editor(&path("b.rs")), editor(&path("c.rs"))], 0, false),
        ));
        backend.seed_layout(&workspace_id, initial.clone(), 1);
        render(&workspace, &ade_workspace, initial.clone(), &mut cx).await;
        cx.update(|window, cx| {
            AdeLayouts::install(
                &workspace,
                ade_workspace.clone(),
                initial.clone(),
                1,
                window,
                cx,
            )
        });

        let (left, right, terminal, moved, closed) =
            workspace.read_with(&mut cx, |workspace, cx| {
                let panes = workspace.panes();
                let left = panes.first().expect("the left pane").clone();
                let right = panes.get(1).expect("the right pane").clone();
                let left_items = left.read(cx).items().collect::<Vec<_>>();
                let right_items = right.read(cx).items().collect::<Vec<_>>();
                (
                    left,
                    right,
                    left_items.get(1).expect("the terminal").item_id(),
                    right_items.first().expect("the moved file").item_id(),
                    right_items.get(1).expect("the closed file").item_id(),
                )
            });
        pretend_item_is_session(&workspace, terminal, "doomed-session", &mut cx);
        cx.update(|window, cx| workspace::move_item(&right, &left, moved, 1, true, window, cx));
        right.update_in(&mut cx, |pane, window, cx| {
            pane.remove_item(closed, false, false, window, cx)
        });
        left.update_in(&mut cx, |pane, window, cx| {
            pane.remove_item(terminal, false, false, window, cx)
        });
        cx.run_until_parked();
        let local = workspace
            .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
            .expect("the moved files remain open");

        let scrubbed = LayoutDoc::new(split(
            SplitDir::Horizontal,
            0.5,
            leaf(vec![editor(&path("a.rs"))], 0, true),
            leaf(vec![editor(&path("b.rs")), editor(&path("c.rs"))], 0, false),
        ));
        backend.write_behind_the_clients_back(&workspace_id, scrubbed.clone(), 2);
        let sync = cx.update(|_, cx| {
            AdeLayouts::sync_for(workspace.entity_id(), cx).expect("the layout sync is installed")
        });
        sync.update(&mut cx, |sync, cx| {
            sync.on_layout_event(
                &LayoutEvent {
                    workspace_id: workspace_id.clone(),
                    layout: scrubbed,
                    rev: 2,
                },
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            workspace
                .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
                .expect("the moved files remain open"),
            local,
            "the scrub must not undo a move or restore a closed file in another pane"
        );

        cx.executor().advance_clock(PUSH_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(backend.stored_rev(&workspace_id), Some(3));
        assert_eq!(
            backend
                .layouts
                .lock()
                .unwrap()
                .get(&workspace_id)
                .map(|stored| stored.layout.clone()),
            Some(local)
        );
    }

    #[gpui::test]
    async fn test_closing_the_last_tab_persists_an_empty_layout(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_closing_the_last_tab", &mut cx).await;
        let workspace_id = ade_workspace.daemon_workspace_id();
        let stored = LayoutDoc::new(leaf(vec![editor(&path("a.rs"))], 0, true));
        backend.seed_layout(&workspace_id, stored.clone(), 1);
        render(&workspace, &ade_workspace, stored.clone(), &mut cx).await;
        cx.update(|window, cx| {
            AdeLayouts::install(&workspace, ade_workspace, stored, 1, window, cx)
        });

        let item_id = workspace.read_with(&mut cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .items()
                .next()
                .expect("the file tab")
                .item_id()
        });
        let close = workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.active_pane().update(cx, |pane, cx| {
                pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
            })
        });
        close.await.unwrap();
        cx.executor().advance_clock(PUSH_DEBOUNCE * 2);
        cx.run_until_parked();

        let stored = backend.layouts.lock().unwrap();
        let stored = stored.get(&workspace_id).expect("the persisted layout");
        assert_eq!(stored.rev, 2);
        assert_eq!(stored.layout, LayoutDoc::empty());
    }

    #[gpui::test]
    async fn test_rapid_layout_broadcasts_finish_on_the_latest_revision(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let _backend = install_lifecycle("test_rapid_layout_broadcasts", &mut cx).await;
        let workspace_id = ade_workspace.daemon_workspace_id();
        let initial = LayoutDoc::new(leaf(vec![editor(&path("a.rs"))], 0, true));
        render(&workspace, &ade_workspace, initial.clone(), &mut cx).await;
        cx.update(|window, cx| {
            AdeLayouts::install(&workspace, ade_workspace, initial, 1, window, cx)
        });

        let latest = LayoutDoc::new(leaf(vec![editor(&path("c.rs"))], 0, true));
        let sync = cx.update(|_, cx| {
            AdeLayouts::sync_for(workspace.entity_id(), cx).expect("the layout sync is installed")
        });
        sync.update(&mut cx, |sync, cx| {
            sync.on_layout_event(
                &LayoutEvent {
                    workspace_id: workspace_id.clone(),
                    layout: LayoutDoc::new(leaf(vec![editor(&path("b.rs"))], 0, true)),
                    rev: 2,
                },
                cx,
            );
            sync.on_layout_event(
                &LayoutEvent {
                    workspace_id,
                    layout: latest.clone(),
                    rev: 3,
                },
                cx,
            );
        });
        cx.run_until_parked();

        assert_eq!(sync.read_with(&mut cx, |sync, _| sync.rev()), 3);
        assert_eq!(
            workspace
                .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
                .expect("the latest file is rendered"),
            latest
        );
    }

    #[gpui::test]
    async fn test_a_local_close_during_a_remote_render_is_not_reopened(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_close_during_remote_render", &mut cx).await;
        let workspace_id = ade_workspace.daemon_workspace_id();
        let initial = LayoutDoc::new(leaf(
            vec![editor(&path("a.rs")), editor(&path("b.rs"))],
            0,
            true,
        ));
        backend.seed_layout(&workspace_id, initial.clone(), 1);
        render(&workspace, &ade_workspace, initial.clone(), &mut cx).await;
        cx.update(|window, cx| {
            AdeLayouts::install(&workspace, ade_workspace, initial, 1, window, cx)
        });
        let (live_pane, live_pane_count, closed) = workspace.read_with(&mut cx, |workspace, cx| {
            let pane = workspace.active_pane().clone();
            let closed = pane
                .read(cx)
                .items()
                .nth(1)
                .expect("the second file")
                .item_id();
            (pane, workspace.panes().len(), closed)
        });

        let remote = LayoutDoc::new(leaf(
            vec![terminal("session-one"), editor(&path("c.rs"))],
            0,
            true,
        ));
        backend.write_behind_the_clients_back(&workspace_id, remote.clone(), 2);
        let sync = cx.update(|_, cx| {
            AdeLayouts::sync_for(workspace.entity_id(), cx).expect("the layout sync is installed")
        });
        sync.update(&mut cx, |sync, cx| {
            sync.on_layout_event(
                &LayoutEvent {
                    workspace_id: workspace_id.clone(),
                    layout: remote,
                    rev: 2,
                },
                cx,
            )
        });

        let mut speculative = None;
        for _ in 0..100 {
            speculative = workspace.read_with(&mut cx, |workspace, cx| {
                workspace
                    .panes()
                    .iter()
                    .skip(live_pane_count)
                    .find_map(|pane| {
                        let item_id = pane.read(cx).items().next()?.item_id();
                        Some((pane.clone(), item_id))
                    })
            });
            if speculative.is_some() {
                break;
            }
            assert!(
                cx.executor().tick(),
                "the remote render should make progress"
            );
        }
        assert!(
            speculative.is_some(),
            "the close must happen after a speculative replacement pane has been filled"
        );
        assert!(
            !backend
                .calls()
                .iter()
                .any(|call| call == "attach_session:session-one"),
            "a speculative layout must not start resolving a hidden attach client: {:?}",
            backend.calls()
        );
        live_pane.update_in(&mut cx, |pane, window, cx| {
            pane.remove_item(closed, false, false, window, cx)
        });
        let locally_closed = LayoutDoc::new(leaf(vec![editor(&path("a.rs"))], 0, true));

        cx.run_until_parked();
        assert_eq!(
            workspace
                .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
                .expect("the first file remains open"),
            locally_closed,
            "a render already resolving in the background must not restore a tab closed after it started"
        );
        assert_eq!(
            workspace.read_with(&mut cx, |workspace, _| workspace.panes().len()),
            live_pane_count,
            "the cancelled render must discard every speculative pane"
        );
        let (speculative_pane, speculative_item) = speculative.expect("checked above");
        workspace.read_with(&mut cx, |workspace, _| {
            assert!(
                workspace
                    .pane_for_entity_id(speculative_pane.entity_id())
                    .is_none(),
                "the discarded pane must leave the workspace index"
            );
            assert!(
                workspace.pane_for_item_id(speculative_item).is_none(),
                "the discarded pane's items must leave the item index"
            );
        });

        cx.executor().advance_clock(PUSH_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(backend.stored_rev(&workspace_id), Some(3));
        assert_eq!(
            backend
                .layouts
                .lock()
                .unwrap()
                .get(&workspace_id)
                .map(|stored| stored.layout.clone()),
            Some(locally_closed)
        );
        assert!(
            !backend
                .calls()
                .iter()
                .any(|call| call == "attach_session:session-one"),
            "a discarded layout must never materialize its terminal: {:?}",
            backend.calls()
        );
    }

    #[gpui::test(iterations = 10)]
    async fn test_a_second_local_change_wins_while_the_previous_write_is_in_flight(
        cx: &mut TestAppContext,
    ) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_change_during_layout_write", &mut cx).await;
        let workspace_id = ade_workspace.daemon_workspace_id();
        let initial = LayoutDoc::new(split(
            SplitDir::Horizontal,
            0.5,
            leaf(vec![editor(&path("a.rs"))], 0, true),
            leaf(vec![editor(&path("b.rs")), editor(&path("c.rs"))], 0, false),
        ));
        backend.seed_layout(&workspace_id, initial.clone(), 1);
        render(&workspace, &ade_workspace, initial.clone(), &mut cx).await;
        cx.update(|window, cx| {
            AdeLayouts::install(&workspace, ade_workspace, initial, 1, window, cx)
        });

        let (left, right, moved, closed) = workspace.read_with(&mut cx, |workspace, cx| {
            let panes = workspace.panes();
            let left = panes.first().expect("the left pane").clone();
            let right = panes.get(1).expect("the right pane").clone();
            let items = right.read(cx).items().collect::<Vec<_>>();
            (
                left,
                right,
                items.first().expect("the moved file").item_id(),
                items.get(1).expect("the closed file").item_id(),
            )
        });
        cx.update(|window, cx| workspace::move_item(&right, &left, moved, 1, true, window, cx));

        let sync = cx.update(|_, cx| {
            AdeLayouts::sync_for(workspace.entity_id(), cx).expect("the layout sync is installed")
        });
        sync.update(&mut cx, |sync, cx| sync.push(cx));
        right.update_in(&mut cx, |pane, window, cx| {
            pane.remove_item(closed, false, false, window, cx)
        });
        let latest = workspace
            .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
            .expect("the moved files remain open");

        cx.run_until_parked();
        cx.executor().advance_clock(PUSH_DEBOUNCE * 2);
        cx.run_until_parked();

        assert_eq!(backend.stored_rev(&workspace_id), Some(3));
        assert_eq!(
            backend
                .layouts
                .lock()
                .unwrap()
                .get(&workspace_id)
                .map(|stored| stored.layout.clone()),
            Some(latest),
            "the later close must not be replaced by the earlier move write"
        );
    }

    /// Dragging one must not. Zed moves a tab by removing it from the old pane
    /// and adding it to the new one, and that remove emits the same event a
    /// close does — reading the event alone killed a live session mid-drag, and
    /// the layout push that followed named a session that had just died.
    #[gpui::test]
    async fn test_dragging_a_terminal_tab_to_another_pane_kills_nothing(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_dragging_a_terminal_tab", &mut cx).await;

        let document = LayoutDoc::new(split(
            SplitDir::Vertical,
            0.5,
            leaf(vec![terminal("dragged-session")], 0, true),
            leaf(vec![editor(&path("a.rs"))], 0, false),
        ));
        render(&workspace, &ade_workspace, document.clone(), &mut cx).await;
        cx.update(|window, cx| {
            AdeLayouts::install(&workspace, ade_workspace.clone(), document, 1, window, cx)
        });

        let (from_pane, to_pane, item_id) = workspace.read_with(&mut cx, |workspace, cx| {
            let panes = workspace.panes();
            let from = panes.first().expect("the left pane").clone();
            let to = panes.get(1).expect("the right pane").clone();
            let item_id = from
                .read(cx)
                .items()
                .next()
                .expect("the terminal tab")
                .item_id();
            (from, to, item_id)
        });
        pretend_item_is_session(&workspace, item_id, "dragged-session", &mut cx);

        // What a tab drop ends in, once the drag gesture is over.
        cx.update(|window, cx| {
            workspace::move_item(&from_pane, &to_pane, item_id, 0, true, window, cx)
        });
        cx.run_until_parked();

        assert!(
            to_pane.read_with(&mut cx, |pane, _| {
                pane.items().any(|item| item.item_id() == item_id)
            }),
            "the tab should have landed in the other pane"
        );
        assert!(
            !backend.killed_anything(),
            "a tab that only moved must not reach the daemon with a kill: {:?}",
            backend.calls()
        );
    }

    /// Opens `name` in `pane` and hands back the item it became.
    async fn open_file_in(
        workspace: &Entity<Workspace>,
        pane: &Entity<Pane>,
        name: &str,
        cx: &mut VisualTestContext,
    ) -> EntityId {
        let opened = workspace.update_in(cx, |workspace, window, cx| {
            let project_path = workspace
                .project()
                .read(cx)
                .find_project_path(PathBuf::from(path(name)), cx)
                .expect("the file is in the project");
            workspace.open_path(project_path, Some(pane.downgrade()), true, window, cx)
        });
        let item_id = opened.await.expect("the file opens").item_id();
        cx.run_until_parked();
        item_id
    }

    /// **A refused push must not cost the user a tab.**
    ///
    /// The daemon refuses a document for two quite different reasons: the
    /// caller lost a race, or the document itself was rejected. Only the first
    /// is a reason to re-render, and answering the second the same way rebuilds
    /// the window from a document *older* than what is on screen — destroying
    /// every live tab that document does not name.
    ///
    /// A pane holding a placeholder for a session the daemon no longer has puts
    /// a window permanently in the second case: every push it makes names that
    /// session, and every one is refused. That is how an editor tab dragged
    /// into a terminal pane "just closed" — the drag is only what triggered the
    /// push.
    #[gpui::test]
    async fn test_a_rejected_push_does_not_rebuild_the_window(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_a_rejected_push_keeps_the_tabs", &mut cx).await;
        let workspace_id = ade_workspace.daemon_workspace_id();

        // A terminal pane whose session the backend does not have — no
        // `live_sessions` were declared — beside an editor.
        let stored = LayoutDoc::new(split(
            SplitDir::Vertical,
            0.5,
            leaf(vec![terminal("ghost-session")], 0, true),
            leaf(vec![editor(&path("b.rs"))], 0, false),
        ));
        backend.seed_layout(&workspace_id, stored.clone(), 1);
        render(&workspace, &ade_workspace, stored.clone(), &mut cx).await;
        cx.update(|window, cx| {
            AdeLayouts::install(&workspace, ade_workspace.clone(), stored, 1, window, cx)
        });

        let (terminal_pane, editor_pane) = workspace.read_with(&mut cx, |workspace, _| {
            let panes = workspace.panes();
            (
                panes.first().expect("the terminal pane").clone(),
                panes.get(1).expect("the editor pane").clone(),
            )
        });

        // The user opens a file the stored document has never heard of, and
        // drags its tab onto the terminal pane's tab bar.
        let dragged = open_file_in(&workspace, &editor_pane, "a.rs", &mut cx).await;
        cx.update(|window, cx| {
            workspace::move_item(&editor_pane, &terminal_pane, dragged, 0, true, window, cx)
        });
        cx.run_until_parked();
        cx.executor().advance_clock(PUSH_DEBOUNCE * 2);
        cx.run_until_parked();

        assert!(
            backend
                .calls()
                .iter()
                .any(|call| call.starts_with("update_layout:")),
            "the drag should have produced a push: {:?}",
            backend.calls()
        );
        assert_eq!(
            backend.stored_rev(&workspace_id),
            Some(1),
            "the push names a session the backend does not have, so it is refused"
        );

        let still_open = workspace.read_with(&mut cx, |workspace, cx| {
            workspace
                .panes()
                .iter()
                .any(|pane| pane.read(cx).items().any(|item| item.item_id() == dragged))
        });
        assert!(
            still_open,
            "a refused push must not take the dragged tab with it"
        );
    }

    /// The other half of that ruling: a revision that really has moved on is
    /// still a re-render. Losing a race means somebody else's arrangement is
    /// the current one, and this window has to show it.
    #[gpui::test]
    async fn test_a_push_that_lost_a_race_still_re_renders(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_a_lost_race_still_re_renders", &mut cx).await;
        let workspace_id = ade_workspace.daemon_workspace_id();

        let stored = LayoutDoc::new(leaf(vec![editor(&path("b.rs"))], 0, true));
        backend.seed_layout(&workspace_id, stored.clone(), 1);
        render(&workspace, &ade_workspace, stored.clone(), &mut cx).await;
        cx.update(|window, cx| {
            AdeLayouts::install(&workspace, ade_workspace.clone(), stored, 1, window, cx)
        });

        // Somebody else rearranges the workspace while this window is looking
        // away, so this window's next push is a revision behind.
        let theirs = LayoutDoc::new(leaf(vec![editor(&path("c.rs"))], 0, true));
        backend.write_behind_the_clients_back(&workspace_id, theirs, 2);

        let pane = workspace.read_with(&mut cx, |workspace, _| workspace.active_pane().clone());
        open_file_in(&workspace, &pane, "a.rs", &mut cx).await;
        cx.executor().advance_clock(PUSH_DEBOUNCE * 2);
        cx.run_until_parked();

        let captured = workspace
            .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
            .expect("the window still has a layout");
        let editors: Vec<String> = match &captured.root {
            LayoutNode::Leaf { tabs, .. } => tabs
                .iter()
                .filter_map(|tab| match tab {
                    Tab::Editor { path } => Some(path.clone()),
                    Tab::Terminal { .. } => None,
                })
                .collect(),
            LayoutNode::Split { .. } => Vec::new(),
        };
        assert_eq!(
            editors.len(),
            1,
            "the window should be showing the other client's single tab, not its own: {editors:?}"
        );
        assert!(
            editors[0].ends_with("c.rs"),
            "the re-render should have brought back their arrangement: {editors:?}"
        );
    }

    #[gpui::test]
    async fn test_a_tab_switch_echo_at_a_newer_revision_does_not_reattach(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_a_tab_switch_echo_at_newer_rev", &mut cx).await;
        let workspace_id = ade_workspace.daemon_workspace_id();

        let stored = LayoutDoc::new(leaf(
            vec![editor(&path("a.rs")), editor(&path("b.rs"))],
            0,
            true,
        ));
        backend.seed_layout(&workspace_id, stored.clone(), 1);
        render(&workspace, &ade_workspace, stored.clone(), &mut cx).await;
        cx.update(|window, cx| {
            AdeLayouts::install(&workspace, ade_workspace.clone(), stored, 1, window, cx)
        });

        let pane = workspace.read_with(&mut cx, |workspace, _| workspace.active_pane().clone());
        let original_pane = pane.entity_id();
        pane.update_in(&mut cx, |pane, window, cx| {
            pane.activate_item(1, false, false, window, cx)
        });
        let switched = workspace
            .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
            .expect("the switched tabs still form a layout");

        let sync = cx.update(|_, cx| {
            AdeLayouts::sync_for(workspace.entity_id(), cx).expect("the layout sync is installed")
        });
        sync.update(&mut cx, |sync, cx| sync.push(cx));
        sync.update(&mut cx, |sync, cx| {
            sync.on_layout_event(
                &LayoutEvent {
                    workspace_id,
                    layout: switched,
                    rev: 3,
                },
                cx,
            )
        });
        cx.run_until_parked();

        assert_eq!(
            workspace.read_with(&mut cx, |workspace, _| {
                workspace.active_pane().entity_id()
            }),
            original_pane,
            "the same layout at a newer revision must not rebuild the pane and reattach its terminals"
        );
    }

    #[gpui::test]
    async fn test_another_clients_tab_switch_does_not_rebuild_the_pane(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let _backend = install_lifecycle("test_remote_tab_switch_keeps_pane", &mut cx).await;
        let workspace_id = ade_workspace.daemon_workspace_id();
        let stored = LayoutDoc::new(leaf(
            vec![editor(&path("a.rs")), editor(&path("b.rs"))],
            0,
            true,
        ));
        render(&workspace, &ade_workspace, stored.clone(), &mut cx).await;
        cx.update(|window, cx| {
            AdeLayouts::install(&workspace, ade_workspace, stored, 1, window, cx)
        });

        let original_pane =
            workspace.read_with(&mut cx, |workspace, _| workspace.active_pane().entity_id());
        let sync = cx.update(|_, cx| {
            AdeLayouts::sync_for(workspace.entity_id(), cx).expect("the layout sync is installed")
        });
        sync.update(&mut cx, |sync, cx| {
            sync.on_layout_event(
                &LayoutEvent {
                    workspace_id,
                    layout: LayoutDoc::new(leaf(
                        vec![editor(&path("a.rs")), editor(&path("b.rs"))],
                        1,
                        true,
                    )),
                    rev: 2,
                },
                cx,
            )
        });
        cx.run_until_parked();

        assert_eq!(
            workspace.read_with(&mut cx, |workspace, _| workspace.active_pane().entity_id()),
            original_pane,
            "a selection-only broadcast must not replace the pane and reattach its terminal views"
        );
    }

    #[gpui::test]
    async fn test_a_remote_tab_switch_does_not_hide_the_next_structural_revision(
        cx: &mut TestAppContext,
    ) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let _backend = install_lifecycle("test_switch_then_structural_revision", &mut cx).await;
        let workspace_id = ade_workspace.daemon_workspace_id();
        let initial = LayoutDoc::new(leaf(
            vec![editor(&path("a.rs")), editor(&path("b.rs"))],
            0,
            true,
        ));
        render(&workspace, &ade_workspace, initial.clone(), &mut cx).await;
        cx.update(|window, cx| {
            AdeLayouts::install(&workspace, ade_workspace, initial, 1, window, cx)
        });
        let sync = cx.update(|_, cx| {
            AdeLayouts::sync_for(workspace.entity_id(), cx).expect("the layout sync is installed")
        });

        sync.update(&mut cx, |sync, cx| {
            sync.on_layout_event(
                &LayoutEvent {
                    workspace_id: workspace_id.clone(),
                    layout: LayoutDoc::new(leaf(
                        vec![editor(&path("a.rs")), editor(&path("b.rs"))],
                        1,
                        true,
                    )),
                    rev: 2,
                },
                cx,
            )
        });
        let structural = LayoutDoc::new(leaf(
            vec![
                editor(&path("a.rs")),
                editor(&path("b.rs")),
                editor(&path("c.rs")),
            ],
            2,
            true,
        ));
        sync.update(&mut cx, |sync, cx| {
            sync.on_layout_event(
                &LayoutEvent {
                    workspace_id,
                    layout: structural.clone(),
                    rev: 3,
                },
                cx,
            )
        });
        cx.run_until_parked();

        assert_eq!(
            workspace
                .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
                .expect("the structural revision is rendered"),
            structural,
            "ignoring a remote selection must not make the unchanged local selection look like an unsaved edit"
        );
    }

    #[gpui::test]
    async fn test_catching_up_does_not_restore_a_tab_closed_during_the_debounce(
        cx: &mut TestAppContext,
    ) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_catch_up_keeps_local_close", &mut cx).await;
        let workspace_id = ade_workspace.daemon_workspace_id();
        let initial = LayoutDoc::new(leaf(
            vec![editor(&path("a.rs")), editor(&path("b.rs"))],
            0,
            true,
        ));
        backend.seed_layout(&workspace_id, initial.clone(), 1);
        render(&workspace, &ade_workspace, initial.clone(), &mut cx).await;
        cx.update(|window, cx| {
            AdeLayouts::install(&workspace, ade_workspace.clone(), initial, 1, window, cx)
        });

        let closed = workspace.read_with(&mut cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .items()
                .nth(1)
                .expect("the second file")
                .item_id()
        });
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.active_pane().update(cx, |pane, cx| {
                pane.remove_item(closed, false, false, window, cx)
            })
        });
        let local = workspace
            .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
            .expect("the first file remains open");

        let stored = WorkspaceLayout {
            layout: LayoutDoc::new(leaf(
                vec![
                    editor(&path("a.rs")),
                    editor(&path("b.rs")),
                    editor(&path("c.rs")),
                ],
                2,
                true,
            )),
            rev: 2,
        };
        backend.write_behind_the_clients_back(&workspace_id, stored.layout.clone(), stored.rev);
        assert!(cx.update(|_, cx| {
            AdeLayouts::catch_up_if_showing(workspace.entity_id(), &ade_workspace, &stored, cx)
        }));
        cx.run_until_parked();
        assert_eq!(
            workspace
                .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
                .expect("the first file remains open"),
            local,
            "focusing an already-open workspace must not replace a local close with the last stored layout"
        );

        cx.executor().advance_clock(PUSH_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(backend.stored_rev(&workspace_id), Some(3));
        assert_eq!(
            backend
                .layouts
                .lock()
                .unwrap()
                .get(&workspace_id)
                .map(|stored| stored.layout.clone()),
            Some(local)
        );
    }

    #[gpui::test]
    async fn test_opening_a_file_while_a_broadcast_is_pending_keeps_the_file(
        cx: &mut TestAppContext,
    ) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_open_file_before_broadcast", &mut cx).await;
        let workspace_id = ade_workspace.daemon_workspace_id();
        let initial = LayoutDoc::new(leaf(vec![editor(&path("a.rs"))], 0, true));
        backend.seed_layout(&workspace_id, initial.clone(), 1);
        render(&workspace, &ade_workspace, initial.clone(), &mut cx).await;
        cx.update(|window, cx| {
            AdeLayouts::install(&workspace, ade_workspace, initial, 1, window, cx)
        });

        let pane = workspace.read_with(&mut cx, |workspace, _| workspace.active_pane().clone());
        open_file_in(&workspace, &pane, "b.rs", &mut cx).await;
        let local = workspace
            .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
            .expect("both local files are open");

        let remote = LayoutDoc::new(leaf(vec![editor(&path("c.rs"))], 0, true));
        backend.write_behind_the_clients_back(&workspace_id, remote.clone(), 2);
        let sync = cx.update(|_, cx| {
            AdeLayouts::sync_for(workspace.entity_id(), cx).expect("the layout sync is installed")
        });
        sync.update(&mut cx, |sync, cx| {
            sync.on_layout_event(
                &LayoutEvent {
                    workspace_id: workspace_id.clone(),
                    layout: remote,
                    rev: 2,
                },
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            workspace
                .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
                .expect("both local files are open"),
            local
        );

        cx.executor().advance_clock(PUSH_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(backend.stored_rev(&workspace_id), Some(3));
        assert_eq!(
            backend
                .layouts
                .lock()
                .unwrap()
                .get(&workspace_id)
                .map(|stored| stored.layout.clone()),
            Some(local)
        );
    }

    #[gpui::test]
    async fn test_a_project_diff_tab_survives_its_debounced_layout_echo(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_a_project_diff_survives_its_echo", &mut cx).await;
        let workspace_id = ade_workspace.daemon_workspace_id();

        let stored = LayoutDoc::new(leaf(vec![editor(&path("a.rs"))], 0, true));
        backend.seed_layout(&workspace_id, stored.clone(), 1);
        render(&workspace, &ade_workspace, stored.clone(), &mut cx).await;
        cx.update(|window, cx| {
            AdeLayouts::install(&workspace, ade_workspace.clone(), stored, 1, window, cx)
        });

        let project_path = workspace.read_with(&mut cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .find_project_path(PathBuf::from(path("b.rs")), cx)
                .expect("the changed file is in the project")
        });
        let pane = workspace.read_with(&mut cx, |workspace, _| workspace.active_pane().clone());
        let diff_item = cx.update(|window, cx| {
            let project_item =
                TestProjectItem::new_in_worktree(2, "b.rs", project_path.worktree_id, cx);
            let diff_item = cx.new(|cx| {
                TestItem::new(cx)
                    .with_label("Uncommitted Changes")
                    .with_project_items(&[project_item])
            });
            pane.update(cx, |pane, cx| {
                pane.add_item(Box::new(diff_item.clone()), false, true, None, window, cx)
            });
            diff_item
        });
        let diff_is_open = |cx: &mut VisualTestContext| {
            pane.read_with(cx, |pane, _| {
                pane.items_of_type::<TestItem>()
                    .any(|item| item.entity_id() == diff_item.entity_id())
            })
        };
        assert!(diff_is_open(&mut cx), "the project diff test tab opens");

        let echoed = workspace
            .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
            .expect("the project diff contributes its active file path");
        let sync = cx.update(|_, cx| {
            AdeLayouts::sync_for(workspace.entity_id(), cx).expect("the layout sync is installed")
        });
        sync.update(&mut cx, |sync, cx| sync.push(cx));
        assert!(
            diff_is_open(&mut cx),
            "capturing and starting the write does not close the project diff"
        );
        sync.update(&mut cx, |sync, cx| {
            sync.on_layout_event(
                &LayoutEvent {
                    workspace_id,
                    layout: echoed,
                    rev: 2,
                },
                cx,
            )
        });
        cx.run_until_parked();

        assert!(
            diff_is_open(&mut cx),
            "the persistence echo must not replace Uncommitted Changes with a plain file editor"
        );
    }

    /// A workspace somebody else killed: this window stops syncing it, so no
    /// later push can write panes back into a record the daemon has deleted.
    #[gpui::test(iterations = 10)]
    async fn test_a_killed_workspace_stops_this_windows_sync(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_a_killed_workspace_stops_the_sync", &mut cx).await;
        let bind = cx.update(|window, cx| {
            window.spawn(cx, {
                let workspace = workspace.downgrade();
                let ade_workspace = ade_workspace.clone();
                async move |cx| {
                    crate::workspace_view::name_window_after_workspace(
                        &workspace,
                        &ade_workspace,
                        cx,
                    )
                }
            })
        });
        bind.await.expect("the workspace should bind");

        let document = LayoutDoc::new(leaf(vec![editor(&path("a.rs"))], 0, true));
        render(&workspace, &ade_workspace, document.clone(), &mut cx).await;
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.set_ade_owns_layout(window, cx)
            });
            AdeLayouts::install(
                &workspace,
                ade_workspace.clone(),
                document.clone(),
                1,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        cx.update(|_, cx| assert_eq!(cx.default_global::<AdeLayouts>().syncs.len(), 1));
        assert!(workspace.read_with(&mut cx, |workspace, _| workspace.ade_owns_layout()));

        // Another workspace's death is not this one's business.
        cx.update(|_, cx| AdeLayouts::forget(None, "ade-somebody-else-000001", cx));
        cx.update(|_, cx| assert_eq!(cx.default_global::<AdeLayouts>().syncs.len(), 1));

        // Session and workspace ids are host-scoped. The same id dying on a
        // remote daemon must not stop this local window's sync.
        cx.update(|_, cx| {
            AdeLayouts::handle_event(
                WorkspaceEvent::Removed {
                    remote_host: Some("h1".to_owned()),
                    workspace_id: ade_workspace.daemon_workspace_id(),
                },
                cx,
            )
        });
        cx.update(|_, cx| assert_eq!(cx.default_global::<AdeLayouts>().syncs.len(), 1));
        assert!(
            workspace.read_with(&mut cx, |workspace, _| workspace.ade_owns_layout()),
            "another host's removal must not release this window"
        );

        cx.update(|_, cx| AdeLayouts::forget(None, &ade_workspace.daemon_workspace_id(), cx));
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert!(
                cx.default_global::<AdeLayouts>().syncs.is_empty(),
                "a killed workspace leaves nothing to sync"
            )
        });
        assert!(
            !workspace.read_with(&mut cx, |workspace, _| workspace.ade_owns_layout()),
            "a killed workspace must return its window to ordinary terminal ownership"
        );
        assert!(workspace.read_with(&mut cx, |workspace, _| {
            workspace.window_title_override().is_none()
        }));
        cx.update(|_, cx| {
            assert!(
                crate::workspace_view::bound_workspace(workspace.entity_id(), cx).is_none(),
                "a killed workspace must release its window binding"
            )
        });

        // Being told a workspace died is not a reason to kill anything: the
        // kill already happened, on the client whose control said so.
        assert!(!backend.killed_anything(), "{:?}", backend.calls());
    }

    #[gpui::test(iterations = 10)]
    async fn test_daemon_incarnation_reset_keeps_the_center_session_manager(
        cx: &mut TestAppContext,
    ) {
        let (workspace, ade_workspace, mut cx) = test_action_window(cx).await;
        let backend = install_lifecycle("test_daemon_reset_keeps_center_sessions", &mut cx).await;
        let initial = LayoutDoc::new(leaf(vec![editor(&path("a.rs"))], 0, true));
        render(&workspace, &ade_workspace, initial.clone(), &mut cx).await;
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.set_ade_owns_layout(window, cx)
            });
            AdeLayouts::install(&workspace, ade_workspace.clone(), initial, 9, window, cx);
        });
        let original_sync = cx.update(|_, cx| {
            AdeLayouts::sync_for(workspace.entity_id(), cx)
                .expect("the original layout sync is installed")
        });

        let reset = LayoutDoc::new(leaf(vec![terminal("still-running")], 0, true));
        cx.update(|_, cx| {
            AdeLayouts::handle_event(
                WorkspaceEvent::Reset {
                    remote_host: None,
                    event: LayoutEvent {
                        workspace_id: ade_workspace.daemon_workspace_id(),
                        layout: reset.clone(),
                        rev: 1,
                    },
                },
                cx,
            )
        });

        let reset_sync = cx.update(|_, cx| {
            AdeLayouts::sync_for(workspace.entity_id(), cx)
                .expect("an incarnation reset must retain the window's session manager")
        });
        assert_eq!(reset_sync.entity_id(), original_sync.entity_id());
        assert_eq!(reset_sync.read_with(&mut cx, |sync, _| sync.rev()), 1);

        // The user can click while the replacement layout is still resolving.
        cx.dispatch_action(NewCenterTerminal::default());
        cx.run_until_parked();

        assert_eq!(
            backend
                .calls()
                .iter()
                .filter(|call| call.starts_with("create_in:"))
                .count(),
            1,
            "the center action must reach the daemon during a reset: {:?}",
            backend.calls()
        );
        let placeholders = workspace.read_with(&mut cx, |workspace, cx| {
            workspace
                .panes()
                .iter()
                .flat_map(|pane| pane.read(cx).items().collect::<Vec<_>>())
                .filter_map(|item| item.downcast::<MissingTab>())
                .map(|item| item.read(cx).tab().clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            placeholders,
            vec![terminal("still-running")],
            "a session that cannot reattach stays represented after the reset"
        );

        let removed_item_id = original_sync.entity_id();
        reset_sync.update(&mut cx, |sync, cx| {
            sync.sessions_by_item
                .insert(removed_item_id, "still-running".to_owned());
            sync.on_workspace_event(
                &workspace::Event::ItemRemoved {
                    item_id: removed_item_id,
                },
                cx,
            );
        });
        cx.run_until_parked();
        assert!(
            !backend.killed_anything(),
            "replacing an unattached terminal with its placeholder is not a user close: {:?}",
            backend.calls()
        );
    }

    /// **N sessions in one workspace**, which is what the daemon has always
    /// held and the client used to collapse to one. A document naming three
    /// terminals across two panes builds all three tabs and captures back as
    /// the same three — including two in one pane, which is what an extra
    /// terminal opened beside the first looks like.
    #[gpui::test]
    async fn test_several_sessions_of_one_workspace_round_trip(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_several_sessions_round_trip", &mut cx).await;

        let document = LayoutDoc::new(split(
            SplitDir::Horizontal,
            0.5,
            leaf(vec![terminal("session-one")], 0, true),
            leaf(
                vec![terminal("session-two"), terminal("session-three")],
                1,
                false,
            ),
        ));
        render(&workspace, &ade_workspace, document.clone(), &mut cx).await;

        workspace.update(&mut cx, |workspace, cx| {
            assert_eq!(workspace.panes().len(), 2, "one pane per leaf");
            let tabs: Vec<usize> = workspace
                .panes()
                .iter()
                .map(|pane| pane.read(cx).items_len())
                .collect();
            assert_eq!(tabs, vec![1, 2], "every terminal tab is built");
        });

        // The backend cannot attach from here, so the tabs are placeholders —
        // an attached terminal needs a pty, which is `daemon_backend`'s side of
        // the seam. What is under test is that three sessions survive the round
        // trip rather than one.
        let captured = workspace
            .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
            .expect("a window showing terminals has a layout");
        assert_eq!(captured, document);
        assert!(!backend.killed_anything(), "{:?}", backend.calls());
    }

    #[gpui::test]
    async fn test_opening_a_file_keeps_the_session_tab(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_window(cx).await;
        let backend = install_lifecycle("test_opening_a_file_keeps_the_session", &mut cx).await;
        let document = LayoutDoc::new(leaf(vec![terminal("session-one")], 0, true));
        render(&workspace, &ade_workspace, document, &mut cx).await;

        let opened = workspace.update_in(&mut cx, |workspace, window, cx| {
            let project_path = workspace
                .project()
                .read(cx)
                .find_project_path(PathBuf::from(path("a.rs")), cx)
                .expect("the file is in the project");
            workspace.open_path(project_path, None, true, window, cx)
        });
        opened.await.expect("the file opens");

        let captured = workspace
            .read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx))
            .expect("the terminal and editor are both nameable");
        assert_eq!(
            captured,
            LayoutDoc::new(leaf(
                vec![terminal("session-one"), editor(&path("a.rs"))],
                1,
                true,
            ))
        );
        assert!(!backend.killed_anything(), "{:?}", backend.calls());
    }

    /// The new-terminal action: a daemon session in a window the daemon owns,
    /// and somebody else's business in a window it does not.
    #[gpui::test]
    async fn test_new_terminal_only_makes_a_session_in_an_ade_window(cx: &mut TestAppContext) {
        let (workspace, mut ade_workspace, mut cx) = test_action_window(cx).await;
        let backend = install_lifecycle("test_new_terminal_makes_a_session", &mut cx).await;

        // An ordinary window: the action is propagated, and nothing here
        // touches the backend. (Zed's own handler is not registered in this
        // harness, so "propagated" shows up as "nothing happened".)
        cx.dispatch_action(NewCenterTerminal::default());
        cx.run_until_parked();
        assert!(backend.calls().is_empty(), "{:?}", backend.calls());

        ade_workspace.repository_path = PathBuf::from("/renamed-away");
        let document = LayoutDoc::new(leaf(vec![editor(&path("a.rs"))], 0, true));
        render(&workspace, &ade_workspace, document.clone(), &mut cx).await;
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.set_ade_owns_layout(window, cx)
            });
            AdeLayouts::install(&workspace, ade_workspace.clone(), document, 1, window, cx);
        });
        cx.run_until_parked();

        cx.dispatch_action(NewCenterTerminal::default());
        cx.run_until_parked();
        assert_eq!(
            backend
                .calls()
                .iter()
                .filter(|call| call.starts_with("create_in:"))
                .collect::<Vec<_>>(),
            vec![&format!(
                "create_in:{}",
                ade_workspace.daemon_workspace_id()
            )],
            "{:?}",
            backend.calls()
        );
        assert_eq!(
            backend.created_working_directories(),
            [PathBuf::from(ROOT)],
            "a new session should use the live worktree root, not stale ADE metadata",
        );
        // A session the client could not even make is not a session it killed.
        assert!(!backend.killed_anything(), "{:?}", backend.calls());
    }

    /// Ownership without a sync — the state [`crate::attach`] leaves behind
    /// when a workspace stores no layout to install one from. The window claims
    /// the centre but has no ADE workspace recorded to create a session *in*,
    /// so the gesture must end there: no backend call, no stock terminal in a
    /// centre the daemon owns, and a notification saying so.
    #[gpui::test]
    async fn test_new_terminal_in_an_ade_window_without_a_sync_creates_nothing(
        cx: &mut TestAppContext,
    ) {
        let (workspace, ade_workspace, mut cx) = test_action_window(cx).await;
        let backend = install_lifecycle("test_new_terminal_without_a_sync", &mut cx).await;

        // Owned, and deliberately never `AdeLayouts::install`ed.
        let document = LayoutDoc::new(leaf(vec![editor(&path("a.rs"))], 0, true));
        render(&workspace, &ade_workspace, document, &mut cx).await;
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.set_ade_owns_layout(window, cx)
            })
        });
        cx.run_until_parked();
        cx.update(|_, cx| assert!(cx.default_global::<AdeLayouts>().syncs.is_empty()));

        let tabs = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .panes()
                    .iter()
                    .map(|pane| pane.read(cx).items_len())
                    .collect::<Vec<_>>()
            })
        };
        let before = tabs(&mut cx);

        cx.dispatch_action(NewCenterTerminal::default());
        cx.run_until_parked();

        assert!(backend.calls().is_empty(), "{:?}", backend.calls());
        assert_eq!(
            tabs(&mut cx),
            before,
            "an unsynced ADE centre gets no terminal at all, stock or otherwise"
        );
        assert_eq!(
            workspace
                .read_with(&mut cx, |workspace, _| workspace.notification_ids())
                .len(),
            1,
            "the inconsistent state is on screen, not only in the log"
        );
    }

    #[gpui::test]
    async fn test_center_terminal_helpers_use_the_ade_session_manager(cx: &mut TestAppContext) {
        let (workspace, ade_workspace, mut cx) = test_action_window(cx).await;
        let backend = install_lifecycle("test_center_terminal_uses_ade_session", &mut cx).await;
        let document = LayoutDoc::new(leaf(vec![editor(&path("a.rs"))], 0, true));
        render(&workspace, &ade_workspace, document.clone(), &mut cx).await;
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.set_ade_owns_layout(window, cx)
            });
            AdeLayouts::install(&workspace, ade_workspace.clone(), document, 1, window, cx);
        });
        cx.run_until_parked();

        let task = workspace.update_in(&mut cx, |workspace, window, cx| {
            terminal_panel::TerminalPanel::add_center_terminal(workspace, window, cx, |_, _| {
                Task::ready(Err(anyhow::anyhow!("plain terminal fallback")))
            })
        });
        assert!(task.await.is_err());

        assert_eq!(
            backend
                .calls()
                .iter()
                .filter(|call| call.starts_with("create_in:"))
                .collect::<Vec<_>>(),
            vec![&format!(
                "create_in:{}",
                ade_workspace.daemon_workspace_id()
            )],
            "{:?}",
            backend.calls()
        );
    }

    /// The toggle actions: their opening half makes a daemon session in an
    /// ADE window, and their focusing half — a centre already showing a
    /// terminal — is left to the stock handler.
    #[gpui::test]
    async fn test_toggle_terminal_only_opens_a_session_when_the_centre_has_none(
        cx: &mut TestAppContext,
    ) {
        let (workspace, ade_workspace, mut cx) = test_action_window(cx).await;
        let backend = install_lifecycle("test_toggle_terminal_opens_a_session", &mut cx).await;

        // An ordinary window: the action is propagated, and nothing here
        // touches the backend.
        cx.dispatch_action(terminal_panel::Toggle);
        cx.run_until_parked();
        assert!(backend.calls().is_empty(), "{:?}", backend.calls());

        let document = LayoutDoc::new(leaf(vec![editor(&path("a.rs"))], 0, true));
        render(&workspace, &ade_workspace, document.clone(), &mut cx).await;
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.set_ade_owns_layout(window, cx)
            });
            AdeLayouts::install(&workspace, ade_workspace.clone(), document, 1, window, cx);
        });
        cx.run_until_parked();

        let creates = |backend: &UnreachableBackend| {
            backend
                .calls()
                .iter()
                .filter(|call| call.starts_with("create_in:"))
                .count()
        };

        // An ADE window with no terminal in the centre: the toggle opens one,
        // and it is a daemon session.
        cx.dispatch_action(terminal_panel::Toggle);
        cx.run_until_parked();
        assert_eq!(creates(&backend), 1, "{:?}", backend.calls());

        // With a terminal in the centre the toggle is a focus gesture:
        // propagated, so no second session is made.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                terminal_panel::TerminalPanel::insert_test_center_terminal(
                    workspace,
                    TerminalId::new(),
                    window,
                    cx,
                )
            })
        });
        cx.run_until_parked();
        cx.dispatch_action(terminal_panel::ToggleFocus);
        cx.run_until_parked();
        assert_eq!(creates(&backend), 1, "{:?}", backend.calls());
        assert!(!backend.killed_anything(), "{:?}", backend.calls());
    }

    #[gpui::test]
    async fn test_a_window_with_nothing_nameable_in_it_stores_no_layout(cx: &mut TestAppContext) {
        let (workspace, _ade_workspace, mut cx) = test_window(cx).await;
        // A fresh test window's pane is empty: capturing it must say "nothing"
        // rather than "an empty document", which would overwrite a real one.
        let captured = workspace.read_with(&mut cx, |workspace, cx| capture_layout(workspace, cx));
        assert_eq!(captured, None);
    }

    #[test]
    fn test_a_broadcast_of_ones_own_write_is_not_news() {
        let event = |workspace_id: &str, rev: u64| LayoutEvent {
            workspace_id: workspace_id.to_owned(),
            layout: LayoutDoc::single_terminal(SessionId::new("s1")),
            rev,
        };

        // Another client moved past us: rebuild.
        assert_eq!(
            broadcast_action("ade-main-012345", 4, &event("ade-main-012345", 5)),
            Broadcast::Rerender
        );
        // Our own accepted write, echoed back on the event connection.
        assert_eq!(
            broadcast_action("ade-main-012345", 5, &event("ade-main-012345", 5)),
            Broadcast::Ignore
        );
        // A straggler from before a newer write.
        assert_eq!(
            broadcast_action("ade-main-012345", 6, &event("ade-main-012345", 5)),
            Broadcast::Ignore
        );
        // Somebody else's workspace entirely.
        assert_eq!(
            broadcast_action("ade-main-012345", 1, &event("ade-other-987654", 9)),
            Broadcast::Ignore
        );
    }
}
