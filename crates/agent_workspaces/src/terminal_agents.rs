//! The coding agents ZOrca offers from its new-entry menus.
//!
//! Each preset opens a terminal in the centre pane and types its command in.
//! Nothing here is model-backed — the agent runs as a CLI in the terminal —
//! which is why it outlives the agent panel that used to host it.

use std::time::Duration;

use futures::FutureExt as _;
use gpui::TaskExt as _;
use gpui::{App, Entity, WeakEntity, Window};
use ui::{Color, IconName};
use workspace::Workspace;

const TERMINAL_INIT_COMMAND_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

pub const CODEX_FULL_ACCESS_LABEL: &str = "Codex (Full Access)";
pub const CODEX_FULL_ACCESS_COMMAND: &str = "codex --yolo";
pub const CLAUDE_CODE_FULL_ACCESS_LABEL: &str = "Claude Code (Full Access)";
pub const CLAUDE_CODE_FULL_ACCESS_COMMAND: &str = "claude --dangerously-skip-permissions";
pub const OPENCODE_FULL_ACCESS_LABEL: &str = "OpenCode (Full Access)";
pub const OPENCODE_FULL_ACCESS_COMMAND: &str = "opencode --auto";

/// Coding agents offered directly from the new-entry menus, in display order.
/// Shared so the agent panel toolbar and the sidebar's `+` cannot drift apart.
pub const TERMINAL_AGENT_PRESETS: &[(&str, IconName, &str)] = &[
    (
        CODEX_FULL_ACCESS_LABEL,
        IconName::AiOpenAi,
        CODEX_FULL_ACCESS_COMMAND,
    ),
    (
        CLAUDE_CODE_FULL_ACCESS_LABEL,
        IconName::AiClaude,
        CLAUDE_CODE_FULL_ACCESS_COMMAND,
    ),
    (
        OPENCODE_FULL_ACCESS_LABEL,
        IconName::AiOpenCode,
        OPENCODE_FULL_ACCESS_COMMAND,
    ),
];

pub fn append_terminal_agents(
    mut menu: ui::ContextMenu,
    workspace: WeakEntity<Workspace>,
    _window: &mut Window,
    _cx: &mut App,
) -> ui::ContextMenu {
    menu = menu.separator().header("Terminal Agents");
    for (label, icon, command) in TERMINAL_AGENT_PRESETS {
        menu = menu.item(
            ui::ContextMenuEntry::new(*label)
                .icon(*icon)
                .icon_color(Color::Muted)
                .handler({
                    let workspace = workspace.clone();
                    move |window, cx| {
                        let Some(workspace) = workspace.upgrade() else {
                            return;
                        };
                        spawn_center_agent_terminal(&workspace, command, window, cx);
                    }
                }),
        );
    }
    menu
}

/// Opens an agent preset as a center terminal and hands it the preset's command.
pub fn spawn_center_agent_terminal(
    workspace: &Entity<Workspace>,
    command: &'static str,
    window: &mut Window,
    cx: &mut App,
) {
    workspace.update(cx, |workspace, cx| {
        let working_directory = terminal_view::default_working_directory(workspace, cx);
        let terminal = terminal_view::terminal_panel::TerminalPanel::add_center_terminal(
            workspace,
            window,
            cx,
            move |project, cx| project.create_terminal_shell(working_directory, cx),
        );
        cx.spawn(async move |_, cx| {
            let terminal = terminal.await?;
            let Some(terminal) = terminal.upgrade() else {
                return anyhow::Ok(());
            };
            cx.update(|cx| write_terminal_init_command(&terminal, command.to_owned(), cx));
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    });
}

/// Delivers an agent preset's init command to a terminal, wherever it lives.
///
/// A real PTY has to be given time to finish starting up, or the command is
/// typed into a shell that is not listening yet. This is the only path that
/// carries a preset's command, so a center terminal needs it just as the
/// panel's did.
pub fn write_terminal_init_command(
    terminal: &Entity<terminal::Terminal>,
    command: String,
    cx: &mut App,
) {
    if !terminal.read(cx).is_pty() {
        terminal.update(cx, |terminal, _| {
            terminal.write_init_command(terminal_init_command_input(command))
        });
        return;
    }

    let startup = terminal.update(cx, |terminal, _| {
        terminal.start_init_command_startup_handshake()
    });

    let terminal = terminal.downgrade();
    cx.spawn(async move |cx| {
        // Fall back to the timeout so the init command is still delivered if
        // the shell never echoes the marker.
        let timeout = cx
            .background_executor()
            .timer(TERMINAL_INIT_COMMAND_STARTUP_TIMEOUT);
        futures::select_biased! {
            _ = startup.fuse() => {}
            _ = timeout.fuse() => {}
        }

        let input = terminal_init_command_input(command);
        if let Err(error) = terminal.update(cx, move |terminal, cx| {
            if !terminal.write_init_command_after_startup(input, cx) {
                log::debug!(
                    "skipping terminal init command because the terminal is no longer eligible"
                );
            }
        }) {
            log::debug!("skipping terminal init command because the terminal closed: {error}");
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn terminal_init_command_input(command: String) -> Vec<u8> {
    let mut input = command.into_bytes();
    // CR, not "\r\n": "\r\n" puts PowerShell into continuation
    // mode (same convention as the activation-script writes in
    // `TerminalBuilder::new`).
    input.push(b'\x0d');
    input
}
