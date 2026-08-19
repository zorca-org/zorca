//! The one consumer of the daemon's status stream, and the one thing every ADE
//! view observes.
//!
//! **Why this exists.** [`WorkspaceLifecycleService::subscribe_status`] hands
//! out a stream, and the merged stream has a single sender: a second subscriber
//! displaces the first, so whichever view subscribed earlier stops seeing
//! status. With two surfaces on the same data — the scaffold panel and the
//! ledger sidebar — that is a bug waiting on a second window. So the stream is
//! consumed exactly once, here, in a process-wide global; the reconciled
//! entries live on this entity, and every view observes it with
//! [`gpui::Context::observe`]. Adding a third surface costs nothing.
//!
//! **Blocking.** The lifecycle service drives the session backend and reads
//! sqlite, so every call into it goes through the background executor — the same
//! rule that holds everywhere else in this crate.

use crate::{
    AdeWorkspace, AdeWorkspaceRegistry, DaemonKey, SessionState, StatusDelivery, WorkspaceEntry,
    WorkspaceId, WorkspaceLifecycleService,
};
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, Global, SharedString, Subscription, Task,
    WeakEntity,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use terminal::Terminal;
use terminal_view::TerminalView;

struct GlobalWorkspaceStore(Entity<AdeWorkspaceStore>);

impl Global for GlobalWorkspaceStore {}

/// What the session backend last said about every registered workspace, kept
/// current from the daemon's pushed status events.
pub struct AdeWorkspaceStore {
    lifecycle: Arc<WorkspaceLifecycleService>,
    /// The merged view: rows this client uses, then what the hosts hold that it
    /// does not.
    entries: Vec<WorkspaceEntry>,
    /// One line per host the last pass could not reach. Their entries are still
    /// listed, showing what was last known of them.
    host_errors: Vec<SharedString>,
    /// Which host spellings the last pass found to be one daemon — see
    /// [`crate::Reconciled::daemons`]. The views key selection and dedupe by
    /// it, so two spellings of one host do not draw one workspace twice.
    daemons: HashMap<Option<String>, String>,
    /// The last failed refresh, cleared by the next successful one.
    error: Option<SharedString>,
    /// Set when the backend pushes status but the stream could not be opened.
    /// Unlike [`Self::error`] this is never cleared: without the stream the
    /// dots only move when the user acts, and that is a standing condition, not
    /// a failed action.
    status_stream_error: Option<SharedString>,
    /// The title each workspace's session last set for itself — the OSC 0/2
    /// window title of the program running inside, e.g. Claude Code's live
    /// session summary. Fed by [`Self::follow_session_title`]; absent until
    /// the session sets one, and cleared when the session goes away.
    ///
    /// The [`EntityId`] is the terminal the title came from, and it gates
    /// *clearing*: alacritty re-broadcasts a terminal's title state — which
    /// may be "no title" — on every focus change, so a titleless sibling
    /// terminal in the same workspace would otherwise wipe the title the
    /// session terminal set, on every switch between the two.
    session_titles: HashMap<WorkspaceId, (EntityId, SharedString)>,
    /// One watcher per attached session terminal, keyed by the terminal's
    /// entity so a reattach replaces its predecessor. The weak handle is only
    /// for pruning: the pane owns the terminal's lifetime, and a dead entry
    /// here must not accumulate.
    title_watchers: HashMap<EntityId, (WeakEntity<Terminal>, Subscription)>,
    refresh_task: Task<()>,
    /// **Only the latest refresh may land.** A pass takes a whole listing per
    /// host, so one in flight when a workspace is removed is holding a view of
    /// the world from before the removal; landing it puts the dead row back,
    /// and for a workspace with no live session nothing pushes a status event
    /// afterwards to correct it. Every refresh captures this at spawn and
    /// applies only if it is still the one it captured; every removal bumps it.
    refresh_ticket: u64,
    /// The `(host, wire id)` of removals whose registry half is still running.
    ///
    /// The ticket alone cannot cover them: a refresh may *start* after the
    /// synchronous eviction and still land before the mutation finishes, and
    /// its listing is as stale as the earlier one's. Anything named here is
    /// filtered out of a landing refresh until the mutation ends.
    pending_removals: HashSet<(Option<String>, String)>,
    _status_task: Task<()>,
}

/// Windows shells announce their own executable path as the console title on
/// startup; as a tab name that is noise. Show the shell's name instead.
// ponytail: any title ending in a known shell exe collapses; tighten to
// whole-path-only if a real title ever ends that way.
fn friendly_shell_title(title: &str) -> &str {
    let name = title.rsplit(['\\', '/']).next().unwrap_or(title);
    match name.to_ascii_lowercase().as_str() {
        "powershell.exe" | "pwsh.exe" => "PowerShell",
        "cmd.exe" => "Command Prompt",
        // The console host, not a shell: no title at all, so the tab keeps
        // the workspace's name until the program inside speaks.
        "conhost.exe" | "openconsole.exe" => "",
        _ => title,
    }
}

#[cfg(test)]
mod title_tests {
    use super::friendly_shell_title;

    #[test]
    fn a_shell_path_title_becomes_the_shell_name() {
        assert_eq!(
            friendly_shell_title(r"C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe"),
            "PowerShell"
        );
        assert_eq!(
            friendly_shell_title(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            "PowerShell"
        );
        assert_eq!(friendly_shell_title("cmd.exe"), "Command Prompt");
        assert_eq!(friendly_shell_title(r"C:\WINDOWS\system32\conhost.exe"), "");
        assert_eq!(friendly_shell_title("Claude Code"), "Claude Code");
        assert_eq!(friendly_shell_title(""), "");
    }
}

impl AdeWorkspaceStore {
    /// The store for the process, created — and first refreshed — on first use.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if !cx.has_global::<GlobalWorkspaceStore>() {
            let store = cx.new(Self::new);
            cx.set_global(GlobalWorkspaceStore(store));
        }
        cx.global::<GlobalWorkspaceStore>().0.clone()
    }

    /// The store, but only if something has already asked for it. For a reader
    /// that holds `&App` and must not bring the daemon up on its own.
    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalWorkspaceStore>()
            .map(|global| global.0.clone())
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let lifecycle = crate::lifecycle_service(cx);
        // How the entries are kept current is the backend's to say. A polling
        // backend gets a timer; a pushing one gets a reader and *no* timer,
        // because a timer beside the pushes would be exactly the polling the
        // daemon exists to end.
        let (status_task, status_stream_error) = match lifecycle.status_delivery() {
            StatusDelivery::Poll {
                interval: refresh_interval,
            } => (
                cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(refresh_interval).await;
                        if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                            break;
                        }
                    }
                }),
                None,
            ),
            StatusDelivery::Push => match lifecycle.subscribe_status() {
                Ok(events) => (
                    cx.spawn(async move |this, cx| {
                        while events.recv().await.is_ok() {
                            // A burst — a session created, then its first
                            // status — is one refresh, not one per event:
                            // reconciling is a whole listing either way.
                            while events.try_recv().is_ok() {}
                            if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                                break;
                            }
                        }
                    }),
                    None,
                ),
                Err(error) => (
                    Task::ready(()),
                    Some(SharedString::from(format!(
                        "status updates are off: {error:#}"
                    ))),
                ),
            },
        };

        let mut this = Self {
            lifecycle,
            entries: Vec::new(),
            host_errors: Vec::new(),
            daemons: HashMap::new(),
            error: None,
            status_stream_error,
            session_titles: HashMap::new(),
            title_watchers: HashMap::new(),
            refresh_task: Task::ready(()),
            refresh_ticket: 0,
            pending_removals: HashSet::new(),
            _status_task: status_task,
        };
        this.refresh(cx);
        this
    }

    /// Every workspace the last pass saw — used and merely discovered — with
    /// the state its session was last found in, most recently opened first.
    pub fn entries(&self) -> &[WorkspaceEntry] {
        &self.entries
    }

    /// Only the ones with a registry row, for a caller whose actions all take a
    /// [`WorkspaceId`].
    pub fn persisted(&self) -> impl Iterator<Item = (&AdeWorkspace, SessionState)> {
        self.entries.iter().filter_map(WorkspaceEntry::persisted)
    }

    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    pub fn host_errors(&self) -> &[SharedString] {
        &self.host_errors
    }

    pub fn daemons(&self) -> &HashMap<Option<String>, String> {
        &self.daemons
    }

    pub fn status_stream_error(&self) -> Option<&SharedString> {
        self.status_stream_error.as_ref()
    }

    pub fn is_refreshing(&self) -> bool {
        !self.refresh_task.is_ready()
    }

    pub fn lifecycle(&self) -> &Arc<WorkspaceLifecycleService> {
        &self.lifecycle
    }

    pub fn registry(&self) -> &AdeWorkspaceRegistry {
        self.lifecycle.registry()
    }

    /// Follows the session terminal's own title and remembers which terminal
    /// owns it.
    ///
    /// The title is the OSC 0/2 string the program inside sets — Claude Code
    /// keeps its session summary there — which the terminal surfaces as
    /// breadcrumb text, never as its tab title. Every change lands in the map
    /// and is mirrored onto the terminal's tab. A *reset* title clears both:
    /// the map entry goes, and the tab's custom title is dropped so it falls
    /// back to its spawn task's label, which is the workspace's name.
    ///
    /// A workspace with several session terminals is titled by whichever one
    /// last spoke a *name* — the same rule tmux uses for a window of panes.
    /// Silence is not speech: only the terminal whose name is showing can
    /// clear it. Zed re-asserts a terminal's title state on every focus
    /// change (`set_cursor_shape` → alacritty's `set_options`, which
    /// re-broadcasts `Title`/`ResetTitle`), so a titleless shell pane beside
    /// a Claude Code pane would otherwise erase the session's name every
    /// time focus crossed between them.
    pub fn follow_session_title(
        &mut self,
        workspace_id: WorkspaceId,
        terminal_view: &Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        self.title_watchers
            .retain(|_, (terminal, _)| terminal.upgrade().is_some());

        let terminal = terminal_view.read(cx).terminal().clone();
        let view = terminal_view.downgrade();
        let subscription = cx.subscribe(
            &terminal,
            move |this, terminal, event: &terminal::Event, cx| {
                if !matches!(event, terminal::Event::BreadcrumbsChanged) {
                    return;
                }
                let title =
                    friendly_shell_title(terminal.read(cx).breadcrumb_text.trim()).to_owned();
                this.set_session_title(&workspace_id, terminal.entity_id(), &title, cx);
                if let Some(view) = view.upgrade() {
                    view.update(cx, |view, cx| {
                        view.set_custom_title((!title.is_empty()).then(|| title.clone()), cx);
                    });
                }
            },
        );
        self.title_watchers
            .insert(terminal.entity_id(), (terminal.downgrade(), subscription));
    }

    fn set_session_title(
        &mut self,
        id: &WorkspaceId,
        author: EntityId,
        title: &str,
        cx: &mut Context<Self>,
    ) {
        let changed = if title.is_empty() {
            // Only the terminal whose title is showing may clear it; a
            // sibling's "I have no title" is not a claim about this one.
            match self.session_titles.get(id) {
                Some((owner, _)) if *owner == author => self.session_titles.remove(id).is_some(),
                _ => false,
            }
        } else {
            self.session_titles
                .insert(id.clone(), (author, title.to_owned().into()))
                .is_none_or(|(_, previous)| previous.as_ref() != title)
        };
        if changed {
            cx.notify();
        }
    }

    /// **A workspace that is gone.** Drops it from the view now, then drops the
    /// row and the discovery behind it.
    ///
    /// The in-memory removal is synchronous because the user has just been told
    /// (or has just asked) that the workspace is dead: leaving it on screen
    /// until a round trip to sqlite and the host finishes is a row that can be
    /// clicked. The refresh that follows is what corrects everything else.
    pub fn forget_workspace(&mut self, host: Option<&str>, wire_id: &str, cx: &mut Context<Self>) {
        self.entries
            .retain(|entry| entry.remote_host() != host || entry.wire_id() != Some(wire_id));
        // Outdates every refresh already in flight, and tombstones the one that
        // may start before the registry half finishes — see the fields.
        self.refresh_ticket += 1;
        let key = (host.map(str::to_owned), wire_id.to_owned());
        self.pending_removals.insert(key.clone());
        cx.notify();

        let lifecycle = self.lifecycle.clone();
        let (host, wire_id) = key.clone();
        cx.spawn(async move |this, cx| {
            let forgotten = cx
                .background_spawn(async move {
                    lifecycle.forget_workspace(host.as_deref(), &wire_id).await
                })
                .await;
            this.update(cx, |this, cx| {
                // The mutation is over: refreshes started during it are stale
                // too, and the tombstone has nothing left to hide.
                this.refresh_ticket += 1;
                this.pending_removals.remove(&key);
                match forgotten {
                    // Not refreshed on failure: a pass that succeeded would
                    // clear this line, and the row it would put back is the
                    // very thing the line is about. The next refresh shows both
                    // again.
                    Err(error) => {
                        this.error = Some(format!("{error:#}").into());
                        cx.notify();
                    }
                    Ok(()) => this.refresh(cx),
                }
            })
            .ok();
        })
        .detach();
    }

    /// Whether this entry names a removal that is still being written.
    ///
    /// Matched by daemon: the entry may be filed under another spelling of the
    /// host the removal was asked for, and it is the same workspace.
    fn removal_pending(&self, entry: &WorkspaceEntry) -> bool {
        let daemon = self.daemon_of(entry.remote_host());
        entry.wire_id().is_some_and(|wire_id| {
            self.pending_removals
                .iter()
                .any(|(host, id)| id == wire_id && self.daemon_of(host.as_deref()) == daemon)
        })
    }

    fn daemon_of(&self, host: Option<&str>) -> DaemonKey {
        match self.daemons.get(&host.map(str::to_owned)) {
            Some(instance) => DaemonKey::Instance(instance.clone()),
            None => DaemonKey::Host(host.map(str::to_owned)),
        }
    }

    /// Asks the session backend what is actually running and replaces the
    /// entries with the answer. Cheap by design (one listing per host), so it
    /// is safe to call after every action; observers are notified once it lands.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let lifecycle = self.lifecycle.clone();
        self.refresh_ticket += 1;
        let ticket = self.refresh_ticket;
        self.refresh_task = cx.spawn(async move |this, cx| {
            // Blocking: drives the session backend and reads sqlite.
            let reconciled = cx
                .background_spawn(async move { lifecycle.reconcile_all().await })
                .await;

            this.update(cx, |this, cx| {
                // Something has happened since this pass listed, so its answer
                // is a description of a world that is gone.
                if this.refresh_ticket != ticket {
                    return;
                }

                match reconciled {
                    Ok(reconciled) => {
                        this.error = None;
                        // Replaced wholesale, not appended to: a host that came
                        // back must stop being complained about.
                        this.host_errors = reconciled
                            .host_errors
                            .iter()
                            .map(|(host, message)| {
                                SharedString::from(format!("host {host}: {message}"))
                            })
                            .collect();
                        this.daemons = reconciled.daemons;
                        this.entries = reconciled
                            .entries
                            .into_iter()
                            .filter(|entry| !this.removal_pending(entry))
                            .collect();
                        // A title outlives its watcher — the session runs on in
                        // the daemon with every window closed — but not its
                        // session: a workspace whose session is gone reads by
                        // its own name again. `Unknown` keeps the title: the
                        // host merely could not be asked, and nothing is
                        // claimed about the session.
                        let entries = &this.entries;
                        this.session_titles.retain(|id, _| {
                            entries.iter().filter_map(WorkspaceEntry::persisted).any(
                                |(workspace, state)| {
                                    &workspace.id == id
                                        && matches!(
                                            state,
                                            SessionState::Alive | SessionState::Unknown
                                        )
                                },
                            )
                        });
                    }
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
    }
}
