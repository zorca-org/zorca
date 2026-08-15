//! Agent terminal and worktree workflows for ZOrca.
//!
//! Coding agents run as CLIs inside terminals. This crate owns their launch
//! presets, persisted terminal metadata, and worktree archive and restore flow.

pub mod row_display;
pub mod terminal_agents;
pub mod terminal_thread_metadata_store;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod thread_worktree_archive;
pub mod worktree_metadata_store;

pub use crate::row_display::{format_history_entry_timestamp, fuzzy_match_positions};
pub use crate::terminal_agents::{
    CLAUDE_CODE_FULL_ACCESS_COMMAND, CLAUDE_CODE_FULL_ACCESS_LABEL, CODEX_FULL_ACCESS_COMMAND,
    CODEX_FULL_ACCESS_LABEL, OPENCODE_FULL_ACCESS_COMMAND, OPENCODE_FULL_ACCESS_LABEL,
    TERMINAL_AGENT_PRESETS, append_terminal_agents, spawn_center_agent_terminal,
    write_terminal_init_command,
};
pub use crate::worktree_metadata_store::{
    ArchivedGitWorktree, WorktreeMetadataStore, worktree_info_from_thread_paths,
};
pub use terminal_view::TerminalId;
pub use zed_actions::{CreateWorktree, NewWorktreeBranchTarget, SwitchWorktree};

use gpui::{App, actions};

/// Where a terminal was opened from. Kept for telemetry and the sidebar's own
/// bookkeeping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentThreadSource {
    AgentPanel,
    GitPanel,
    Sidebar,
}

impl AgentThreadSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AgentPanel => "agent_panel",
            Self::GitPanel => "git_panel",
            Self::Sidebar => "sidebar",
        }
    }
}

actions!(
    agent,
    [
        /// Opens a new terminal.
        NewTerminalThread,
        /// Closes the selected terminal.
        ArchiveSelectedThread,
        /// Renames the selected terminal.
        RenameSelectedThread,
        /// Toggles the menu for creating a new entry.
        ToggleNewThreadMenu,
        /// Toggles the options menu.
        ToggleOptionsMenu,
    ]
);

pub fn init(cx: &mut App) {
    worktree_metadata_store::WorktreeMetadataStore::init_global(cx);
    terminal_thread_metadata_store::init(cx);
    workspace::register_new_item_menu_extension(cx, terminal_agents::append_terminal_agents);
}
