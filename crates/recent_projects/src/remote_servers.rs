use crate::{
    remote_connections::{
        Connection, RemoteConnectionModal, RemoteConnectionPrompt, RemoteSettings, SshConnection,
        SshConnectionHeader, connect, determine_paths_with_positions, open_remote_project,
    },
    ssh_config::parse_ssh_config_hosts,
};
mod filter;

use dev_container::{
    DevContainerConfig, DevContainerContext, find_devcontainer_configs,
    start_dev_container_with_config,
};
use editor::Editor;
use extension_host::ExtensionStore;
use filter::{FilterData, FilteredServer};
use futures::{FutureExt, StreamExt as _, channel::oneshot, future::Shared};
use gpui::{
    Action, AnyElement, App, ClipboardItem, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, PromptLevel, StatefulInteractiveElement, Subscription, Task, TaskExt,
    WeakEntity, Window, actions, px,
};
use log::{debug, info};
use open_path_prompt::OpenPathDelegate;
use paths::{global_ssh_config_file, user_ssh_config_file};
use picker::{Picker, PickerDelegate, PickerEditorPosition};
use project::{Fs, Project};
use remote::{
    RemoteClient, RemoteConnectionOptions, SshConnectionOptions, WslConnectionOptions,
    remote_client::ConnectionIdentifier,
};
use settings::{
    RemoteProject, RemoteSettingsContent, Settings as _, SettingsStore, update_settings_file,
    watch_config_file,
};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use ui::{
    CommonAnimationExt, HighlightedLabel, IconButtonShape, KeyBinding, ListItem, ListSeparator,
    ModalHeader, Navigable, NavigableEntry, Tooltip, prelude::*,
};
use util::{
    ResultExt,
    command::new_command,
    paths::{PathStyle, RemotePathBuf},
    rel_path::RelPath,
};
use workspace::{
    AppState, DismissDecision, ModalView, MultiWorkspace, OpenLog, OpenOptions, Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::{DetachAndPromptErr, NotificationId},
    open_remote_project_with_existing_connection,
};

actions!(ports, [ToggleFocus]);

pub struct PortsPanel {
    // Deliberately holds no `WeakEntity<Workspace>`: `Panel::enabled` and
    // `Panel::icon_label` run inside `Workspace::toggle_dock`, which already
    // leases the Workspace entity, so reading it here aborts GPUI.
    project: WeakEntity<Project>,
    focus_handle: FocusHandle,
    add_port_editor: Entity<Editor>,
    adding_port: bool,
    add_port_error: Option<SharedString>,
    listening_processes: BTreeMap<u16, SharedString>,
    local_listening_ports: BTreeSet<u16>,
    refresh_task: Task<()>,
    port_action_task: Task<()>,
    _settings_subscription: Subscription,
}

impl PortsPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: gpui::AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            let project = workspace.project().downgrade();
            cx.new(|cx| Self::new(project, window, cx))
        })?;
        panel.update(&mut cx, |panel, cx| panel.refresh(cx));
        Ok(panel)
    }

    fn new(project: WeakEntity<Project>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let add_port_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Port number or address (for example 3000)", window, cx);
            editor
        });
        Self {
            project,
            focus_handle: cx.focus_handle(),
            add_port_editor,
            adding_port: false,
            add_port_error: None,
            listening_processes: BTreeMap::new(),
            local_listening_ports: BTreeSet::new(),
            refresh_task: Task::ready(()),
            port_action_task: Task::ready(()),
            _settings_subscription: cx.observe_global::<SettingsStore>(|this, cx| {
                this.refresh(cx);
                cx.notify();
            }),
        }
    }

    fn active_server(&self, cx: &App) -> Option<(SshServerIndex, SshConnection)> {
        let project = self.project.upgrade()?;
        let RemoteConnectionOptions::Ssh(connection) =
            project.read(cx).remote_connection_options(cx)?
        else {
            return None;
        };
        let index = ssh_server_index(&connection, cx)?;
        let saved = RemoteSettings::get_global(cx)
            .ssh_connections()
            .nth(index.0)?;
        Some((index, saved))
    }

    fn begin_adding_port(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_server(cx).is_none() {
            return;
        }
        self.adding_port = true;
        self.add_port_error = None;
        self.add_port_editor.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn add_port(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((index, _)) = self.active_server(cx) else {
            return;
        };
        let input = self.add_port_editor.read(cx).text(cx);
        let forward = match parse_quick_port_forward(&input) {
            Ok(forward) => forward,
            Err(error) => {
                self.add_port_error = Some(error.to_string().into());
                cx.notify();
                return;
            }
        };
        let Some(project) = self.project.upgrade() else {
            return;
        };
        self.add_port_error = None;
        let fs = project.read(cx).fs().clone();
        let command = project
            .read(cx)
            .remote_client()
            .ok_or_else(|| anyhow::anyhow!("SSH connection is unavailable"))
            .and_then(|client| ssh_forward_control_command(client.read(cx), &forward, "forward"));
        self.port_action_task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let command = command?;
                let output = new_command(command.program)
                    .args(command.args)
                    .envs(command.env)
                    .output()
                    .await?;
                anyhow::ensure!(
                    output.status.success(),
                    "failed to forward port: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                anyhow::Ok(())
            }
            .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(()) => {
                    update_settings_file(fs, cx, move |settings, _| {
                        let Some(connection) = settings
                            .remote
                            .ssh_connections
                            .as_mut()
                            .and_then(|connections| connections.get_mut(index.0))
                        else {
                            return;
                        };
                        connection
                            .port_forwards
                            .get_or_insert_default()
                            .push(forward);
                    });
                    this.add_port_editor
                        .update(cx, |editor, cx| editor.set_text("", window, cx));
                    this.adding_port = false;
                    this.add_port_error = None;
                    cx.notify();
                }
                Err(error) => {
                    this.add_port_error = Some(error.to_string().into());
                    cx.notify();
                }
            })
            .ok();
        });
    }

    fn remove_port(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some((server_index, _)) = self.active_server(cx) else {
            return;
        };
        let Some(project) = self.project.upgrade() else {
            return;
        };
        let fs = project.read(cx).fs().clone();
        let Some(forward) = self
            .active_server(cx)
            .and_then(|(_, server)| server.port_forwards?.get(index).cloned())
        else {
            return;
        };
        let command = project
            .read(cx)
            .remote_client()
            .ok_or_else(|| anyhow::anyhow!("SSH connection is unavailable"))
            .and_then(|client| ssh_forward_control_command(client.read(cx), &forward, "cancel"));
        self.port_action_task = cx.spawn(async move |this, cx| {
            let result = async {
                let command = command?;
                let output = new_command(command.program)
                    .args(command.args)
                    .envs(command.env)
                    .output()
                    .await?;
                anyhow::ensure!(
                    output.status.success(),
                    "failed to stop forwarding port: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                anyhow::Ok(())
            }
            .await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    update_settings_file(fs, cx, move |settings, _| {
                        let Some(forwards) = settings
                            .remote
                            .ssh_connections
                            .as_mut()
                            .and_then(|connections| connections.get_mut(server_index.0))
                            .and_then(|connection| connection.port_forwards.as_mut())
                        else {
                            return;
                        };
                        if index < forwards.len() {
                            forwards.remove(index);
                        }
                    });
                    this.add_port_error = None;
                    cx.notify();
                }
                Err(error) => {
                    this.add_port_error = Some(error.to_string().into());
                    cx.notify();
                }
            })
            .ok();
        });
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project.upgrade() else {
            self.listening_processes.clear();
            self.local_listening_ports.clear();
            return;
        };
        if project.read(cx).remote_client().is_none() {
            self.listening_processes.clear();
            self.local_listening_ports.clear();
            return;
        }
        let command = project.update(cx, |project, cx| {
            project.exec_in_shell(
                "if command -v ss >/dev/null 2>&1; then ss -ltnpH; elif command -v lsof >/dev/null 2>&1; then lsof -nP -iTCP -sTCP:LISTEN; fi"
                    .to_owned(),
                cx,
            )
        });
        let local_ports = self
            .active_server(cx)
            .and_then(|(_, server)| server.port_forwards)
            .unwrap_or_default()
            .into_iter()
            .map(|forward| forward.local_port)
            .collect::<Vec<_>>();
        let local_listeners = cx.background_spawn(async move {
            local_ports
                .into_iter()
                .filter(|port| {
                    std::net::TcpStream::connect_timeout(
                        &std::net::SocketAddr::from(([127, 0, 0, 1], *port)),
                        Duration::from_millis(100),
                    )
                    .is_ok()
                })
                .collect::<BTreeSet<_>>()
        });
        self.refresh_task = cx.spawn(async move |this, cx| {
            let processes = match command.await {
                Ok(mut command) => match command.output().await {
                    Ok(output) if output.status.success() => {
                        parse_listening_processes(&String::from_utf8_lossy(&output.stdout))
                    }
                    Ok(output) => {
                        log::warn!(
                            "failed to inspect remote listening ports: {}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                        BTreeMap::new()
                    }
                    Err(error) => {
                        log::warn!("failed to inspect remote listening ports: {error:#}");
                        BTreeMap::new()
                    }
                },
                Err(error) => {
                    log::warn!("failed to build remote port inspection command: {error:#}");
                    BTreeMap::new()
                }
            };
            let local_listening_ports = local_listeners.await;
            this.update(cx, |this, cx| {
                this.listening_processes = processes;
                this.local_listening_ports = local_listening_ports;
                cx.notify();
            })
            .ok();
            cx.background_executor().timer(Duration::from_secs(2)).await;
            this.update(cx, |this, cx| this.refresh(cx)).ok();
        });
    }
}

fn parse_listening_processes(output: &str) -> BTreeMap<u16, SharedString> {
    let mut processes = BTreeMap::new();
    for line in output.lines().filter(|line| !line.starts_with("COMMAND")) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        let (address, process) = if fields.first() == Some(&"LISTEN") {
            let Some(address) = fields.get(3) else {
                continue;
            };
            let process = line
                .split_once("users:")
                .map(|(_, process)| process.trim().trim_matches(['(', ')']))
                .unwrap_or_default()
                .to_owned();
            ((*address).to_owned(), process)
        } else {
            let Some(address) = fields.iter().find(|field| field.contains(':')) else {
                continue;
            };
            let process = match (fields.first(), fields.get(1)) {
                (Some(command), Some(pid)) => format!("{command} (PID {pid})"),
                _ => String::new(),
            };
            ((*address).to_owned(), process)
        };
        let Some(port) = address
            .rsplit(':')
            .next()
            .and_then(|port| port.parse::<u16>().ok())
        else {
            continue;
        };
        let process = if let Some(name) = process
            .strip_prefix('"')
            .and_then(|process| process.split_once("\"").map(|(name, _)| name))
        {
            let pid = process
                .split_once("pid=")
                .and_then(|(_, pid)| pid.split_once(',').map(|(pid, _)| pid));
            pid.map_or_else(|| name.to_owned(), |pid| format!("{name} (PID {pid})"))
        } else {
            process
        };
        processes.entry(port).or_insert_with(|| process.into());
    }
    processes
}

fn parse_quick_port_forward(input: &str) -> anyhow::Result<remote::SshPortForwardOption> {
    let input = input.trim();
    let (host, port) = match input.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => (host.trim_matches(['[', ']']), port),
        _ => ("localhost", input),
    };
    let port = port
        .parse::<u16>()
        .map_err(|_| anyhow::anyhow!("Enter a valid port from 1 to 65535"))?;
    anyhow::ensure!(port > 0, "Enter a valid port from 1 to 65535");
    Ok(remote::SshPortForwardOption {
        local_host: Some("localhost".to_owned()),
        local_port: port,
        remote_host: Some(host.to_owned()),
        remote_port: port,
    })
}

fn ssh_forward_control_command(
    client: &RemoteClient,
    forward: &remote::SshPortForwardOption,
    operation: &str,
) -> anyhow::Result<remote::CommandTemplate> {
    let mut command = client.build_forward_ports_command(vec![(
        forward.local_port,
        forward
            .remote_host
            .clone()
            .unwrap_or_else(|| "localhost".to_owned()),
        forward.remote_port,
    )])?;
    let forward_option_index = command
        .args
        .iter()
        .rposition(|argument| argument == "-L")
        .ok_or_else(|| anyhow::anyhow!("SSH forwarding command is missing -L"))?;
    let forward_specification = command
        .args
        .get_mut(forward_option_index + 1)
        .ok_or_else(|| anyhow::anyhow!("SSH forwarding command is missing the -L value"))?;
    *forward_specification = format_port_forward_specification(forward);
    set_ssh_control_operation(&mut command, operation)?;
    Ok(command)
}

fn format_port_forward_specification(forward: &remote::SshPortForwardOption) -> String {
    fn bracket_ipv6(host: &str) -> Cow<'_, str> {
        if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]").into()
        } else {
            host.into()
        }
    }

    let local_host = forward.local_host.as_deref().unwrap_or("localhost");
    let remote_host = forward.remote_host.as_deref().unwrap_or("localhost");
    format!(
        "{}:{}:{}:{}",
        bracket_ipv6(local_host),
        forward.local_port,
        bracket_ipv6(remote_host),
        forward.remote_port
    )
}

fn set_ssh_control_operation(
    command: &mut remote::CommandTemplate,
    operation: &str,
) -> anyhow::Result<()> {
    let option_index = command
        .args
        .iter()
        .rposition(|argument| argument == "-N")
        .ok_or_else(|| anyhow::anyhow!("SSH forwarding command is missing -N"))?;
    command.args.splice(
        option_index..=option_index,
        ["-O".to_owned(), operation.to_owned()],
    );
    Ok(())
}

impl Render for PortsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let server = self.active_server(cx);
        let forwards = server
            .as_ref()
            .and_then(|(_, server)| server.port_forwards.as_ref())
            .cloned()
            .unwrap_or_default();
        let listening_processes = self.listening_processes.clone();
        let local_listening_ports = self.local_listening_ports.clone();
        let has_server = server.is_some();

        v_flex()
            .id("ports-panel")
            .size_full()
            .track_focus(&self.focus_handle)
            .child(
                h_flex()
                    .h_9()
                    .px_3()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Label::new("Ports").size(LabelSize::Small)),
                    )
                    .child(
                        Button::new("add-port", "Add Port")
                            .label_size(LabelSize::Small)
                            .disabled(!has_server)
                            .tooltip(Tooltip::text(if has_server {
                                "Forward a port"
                            } else {
                                "Open an SSH workspace to forward ports"
                            }))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.begin_adding_port(window, cx)
                            })),
                    ),
            )
            .child(
                h_flex()
                    .h_8()
                    .px_3()
                    .gap_3()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(div().w_4())
                    .child(
                        div()
                            .w_24()
                            .child(Label::new("Port").size(LabelSize::Small)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(Label::new("Forwarded Address").size(LabelSize::Small)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(Label::new("Running Process").size(LabelSize::Small)),
                    ),
            )
            .child(
                v_flex()
                    .id("ports-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .when(forwards.is_empty(), |this| {
                        this.child(
                            h_flex().p_4().child(
                                Label::new(if has_server {
                                    "No forwarded ports. Select Add Port to create one."
                                } else {
                                    "Port forwarding is available in SSH workspaces."
                                })
                                .color(Color::Muted),
                            ),
                        )
                    })
                    .children(forwards.into_iter().enumerate().map(|(index, forward)| {
                        let local_host = forward.local_host.as_deref().unwrap_or("localhost");
                        let address = format!("{local_host}:{}", forward.local_port);
                        let address_for_open = address.clone();
                        let process = listening_processes.get(&forward.remote_port).cloned();
                        let is_listening = local_listening_ports.contains(&forward.local_port)
                            && process.is_some();
                        h_flex()
                            .id(("port-forward-row", index))
                            .group("port-forward-row")
                            .h_8()
                            .px_3()
                            .gap_3()
                            .border_b_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(div().w_4().child(
                                Icon::new(IconName::Circle).size(IconSize::XSmall).color(
                                    if is_listening {
                                        Color::Success
                                    } else {
                                        Color::Muted
                                    },
                                ),
                            ))
                            .child(div().w_24().child(forward.local_port.to_string()))
                            .child(
                                div().flex_1().child(
                                    h_flex()
                                        .justify_between()
                                        .child(Label::new(address).color(Color::Accent))
                                        .child(
                                            h_flex()
                                                .visible_on_hover("port-forward-row")
                                                .child(
                                                    IconButton::new(
                                                        ("open-forwarded-port", index),
                                                        IconName::ToolWeb,
                                                    )
                                                    .icon_size(IconSize::Small)
                                                    .tooltip(Tooltip::text("Open in Browser"))
                                                    .on_click(move |_, _, cx| {
                                                        cx.open_url(&format!(
                                                            "http://{address_for_open}"
                                                        ));
                                                    }),
                                                )
                                                .child(
                                                    IconButton::new(
                                                        ("remove-forwarded-port", index),
                                                        IconName::Close,
                                                    )
                                                    .icon_size(IconSize::Small)
                                                    .tooltip(Tooltip::text("Stop Forwarding Port"))
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.remove_port(index, cx)
                                                    })),
                                                ),
                                        ),
                                ),
                            )
                            .child(div().flex_1().when_some(process, |this, process| {
                                this.child(Label::new(process).truncate())
                            }))
                    }))
                    .when(self.adding_port, |this| {
                        this.child(
                            h_flex()
                                .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                                    this.add_port(window, cx)
                                }))
                                .h_8()
                                .px_3()
                                .gap_3()
                                .bg(cx.theme().colors().element_background)
                                .child(div().w_4())
                                .child(div().w_24().child(self.add_port_editor.clone())),
                        )
                    }),
            )
            .when_some(self.add_port_error.clone(), |this, error| {
                this.child(
                    h_flex()
                        .px_3()
                        .py_1()
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
    }
}

impl Focusable for PortsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for PortsPanel {}

impl Panel for PortsPanel {
    fn persistent_name() -> &'static str {
        "Ports Panel"
    }

    fn panel_key() -> &'static str {
        "PortsPanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Bottom
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Bottom
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(260.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Server)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Ports")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        ToggleFocus.boxed_clone()
    }

    fn icon_label(&self, _window: &Window, cx: &App) -> Option<String> {
        self.active_server(cx)
            .and_then(|(_, server)| server.port_forwards)
            .map(|forwards| forwards.len().to_string())
    }

    fn activation_priority(&self) -> u32 {
        6
    }

    fn enabled(&self, cx: &App) -> bool {
        self.active_server(cx).is_some()
    }
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<PortsPanel>(window, cx);
        });
    })
    .detach();
}

pub struct RemoteServerProjects {
    mode: Mode,
    focus_handle: FocusHandle,
    default_picker: Entity<Picker<RemoteServerPickerDelegate>>,
    workspace: WeakEntity<Workspace>,
    #[cfg(target_os = "windows")]
    retained_connections: Vec<Entity<RemoteClient>>,
    ssh_config_updates: Task<()>,
    ssh_config_servers: BTreeSet<SharedString>,
    create_new_window: bool,
    dev_container_picker: Option<Entity<Picker<DevContainerPickerDelegate>>>,
    _subscriptions: Vec<Subscription>,
    allow_dismissal: bool,
}

struct CreateRemoteServer {
    address_editor: Entity<Editor>,
    address_error: Option<SharedString>,
    ssh_prompt: Option<Entity<RemoteConnectionPrompt>>,
    _creating: Option<Task<Option<()>>>,
}

impl CreateRemoteServer {
    fn new(window: &mut Window, cx: &mut App) -> Self {
        let address_editor = cx.new(|cx| Editor::single_line(window, cx));
        address_editor.update(cx, |this, cx| {
            this.focus_handle(cx).focus(window, cx);
        });
        Self {
            address_editor,
            address_error: None,
            ssh_prompt: None,
            _creating: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum DevContainerCreationProgress {
    SelectingConfig,
    Creating,
    Error(String),
}

#[derive(Clone)]
struct CreateRemoteDevContainer {
    view_logs_entry: NavigableEntry,
    back_entry: NavigableEntry,
    progress: DevContainerCreationProgress,
}

impl CreateRemoteDevContainer {
    fn new(progress: DevContainerCreationProgress, cx: &mut Context<RemoteServerProjects>) -> Self {
        let view_logs_entry = NavigableEntry::focusable(cx);
        let back_entry = NavigableEntry::focusable(cx);
        Self {
            view_logs_entry,
            back_entry,
            progress,
        }
    }
}

#[cfg(target_os = "windows")]
struct AddWslDistro {
    picker: Entity<Picker<crate::wsl_picker::WslPickerDelegate>>,
    connection_prompt: Option<Entity<RemoteConnectionPrompt>>,
    _creating: Option<Task<()>>,
}

#[cfg(target_os = "windows")]
impl AddWslDistro {
    fn new(window: &mut Window, cx: &mut Context<RemoteServerProjects>) -> Self {
        use crate::wsl_picker::{WslDistroSelected, WslPickerDelegate, WslPickerDismissed};

        let delegate = WslPickerDelegate::new();
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx).embedded());

        cx.subscribe_in(
            &picker,
            window,
            |this, _, _: &WslDistroSelected, window, cx| {
                this.confirm(&menu::Confirm, window, cx);
            },
        )
        .detach();

        cx.subscribe_in(
            &picker,
            window,
            |this, _, _: &WslPickerDismissed, window, cx| {
                this.cancel(&menu::Cancel, window, cx);
            },
        )
        .detach();

        AddWslDistro {
            picker,
            connection_prompt: None,
            _creating: None,
        }
    }
}

enum ProjectPickerData {
    Ssh {
        connection_string: SharedString,
        nickname: Option<SharedString>,
    },
    Wsl {
        distro_name: SharedString,
    },
}

struct ProjectPicker {
    data: ProjectPickerData,
    picker: Entity<Picker<OpenPathDelegate>>,
    _path_task: Shared<Task<Option<()>>>,
}

struct EditNicknameState {
    index: SshServerIndex,
    editor: Entity<Editor>,
}

fn ssh_server_index(connection: &SshConnectionOptions, cx: &App) -> Option<SshServerIndex> {
    RemoteSettings::get_global(cx)
        .ssh_connections()
        .position(|saved| {
            saved.host == connection.host.to_string()
                && saved.username == connection.username
                && saved.port == connection.port
        })
        .map(SshServerIndex)
}

struct DevContainerPickerDelegate {
    selected_index: usize,
    candidates: Vec<DevContainerConfig>,
    matching_candidates: Vec<DevContainerConfig>,
    parent_modal: WeakEntity<RemoteServerProjects>,
}
impl DevContainerPickerDelegate {
    fn new(
        candidates: Vec<DevContainerConfig>,
        parent_modal: WeakEntity<RemoteServerProjects>,
    ) -> Self {
        Self {
            selected_index: 0,
            matching_candidates: candidates.clone(),
            candidates,
            parent_modal,
        }
    }
}

impl PickerDelegate for DevContainerPickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "remote dev container picker"
    }

    fn match_count(&self) -> usize {
        self.matching_candidates.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select Dev Container Configuration".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query_lower = query.to_lowercase();
        self.matching_candidates = self
            .candidates
            .iter()
            .filter(|c| {
                c.name.to_lowercase().contains(&query_lower)
                    || c.config_path
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&query_lower)
            })
            .cloned()
            .collect();

        self.selected_index = std::cmp::min(
            self.selected_index,
            self.matching_candidates.len().saturating_sub(1),
        );

        Task::ready(())
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let selected_config = self.matching_candidates.get(self.selected_index).cloned();
        self.parent_modal
            .update(cx, move |modal, cx| {
                if secondary {
                    modal.edit_in_dev_container_json(selected_config.clone(), window, cx);
                } else if let Some((app_state, context)) = modal
                    .workspace
                    .read_with(cx, |workspace, cx| {
                        let app_state = workspace.app_state().clone();
                        let context = DevContainerContext::from_workspace(workspace, cx)?;
                        Some((app_state, context))
                    })
                    .ok()
                    .flatten()
                {
                    modal.open_dev_container(selected_config, app_state, context, window, cx);
                    modal.view_in_progress_dev_container(window, cx);
                } else {
                    log::error!("No active project directory for Dev Container");
                }
            })
            .ok();
    }

    fn dismissed(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.parent_modal
            .update(cx, |modal, cx| {
                modal.cancel(&menu::Cancel, window, cx);
            })
            .ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let candidate = self.matching_candidates.get(ix)?;
        let config_path = candidate.config_path.display().to_string();
        Some(
            ListItem::new(SharedString::from(format!("li-devcontainer-config-{}", ix)))
                .inset(true)
                .spacing(ui::ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(Icon::new(IconName::FileToml).color(Color::Muted))
                .child(
                    v_flex().child(Label::new(candidate.name.clone())).child(
                        Label::new(config_path)
                            .size(ui::LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
                .into_any_element(),
        )
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        Some(
            h_flex()
                .w_full()
                .p_1p5()
                .gap_1()
                .justify_start()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    Button::new("run-action", "Start Dev Container")
                        .key_binding(
                            KeyBinding::for_action(&menu::Confirm, cx)
                                .map(|kb| kb.size(rems_from_px(12.))),
                        )
                        .on_click(|_, window, cx| {
                            window.dispatch_action(menu::Confirm.boxed_clone(), cx)
                        }),
                )
                .child(
                    Button::new("run-action-secondary", "Open devcontainer.json")
                        .key_binding(
                            KeyBinding::for_action(&menu::SecondaryConfirm, cx)
                                .map(|kb| kb.size(rems_from_px(12.))),
                        )
                        .on_click(|_, window, cx| {
                            window.dispatch_action(menu::SecondaryConfirm.boxed_clone(), cx)
                        }),
                )
                .into_any_element(),
        )
    }
}

impl EditNicknameState {
    fn new(index: SshServerIndex, window: &mut Window, cx: &mut App) -> Self {
        let this = Self {
            index,
            editor: cx.new(|cx| Editor::single_line(window, cx)),
        };
        let starting_text = RemoteSettings::get_global(cx)
            .ssh_connections()
            .nth(index.0)
            .and_then(|state| state.nickname)
            .filter(|text| !text.is_empty());
        this.editor.update(cx, |this, cx| {
            this.set_placeholder_text("Add a nickname for this server", window, cx);
            if let Some(starting_text) = starting_text {
                this.set_text(starting_text, window, cx);
            }
        });
        this.editor.focus_handle(cx).focus(window, cx);
        this
    }
}

impl Focusable for ProjectPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl ProjectPicker {
    fn new(
        create_new_window: bool,
        index: ServerIndex,
        connection: RemoteConnectionOptions,
        project: Entity<Project>,
        home_dir: RemotePathBuf,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<RemoteServerProjects>,
    ) -> Entity<Self> {
        let (tx, rx) = oneshot::channel();
        let lister = project::DirectoryLister::Project(project.clone());
        let delegate = open_path_prompt::OpenPathDelegate::new(tx, lister, false, cx).show_hidden();

        let picker = cx.new(|cx| {
            let picker = Picker::uniform_list(delegate, window, cx).embedded();
            picker.set_query(&home_dir.to_string(), window, cx);
            picker
        });

        let data = match &connection {
            RemoteConnectionOptions::Ssh(connection) => ProjectPickerData::Ssh {
                connection_string: connection.connection_string().into(),
                nickname: connection.nickname.clone().map(|nick| nick.into()),
            },
            RemoteConnectionOptions::Wsl(connection) => ProjectPickerData::Wsl {
                distro_name: connection.distro_name.clone().into(),
            },
            RemoteConnectionOptions::Docker(_) => ProjectPickerData::Ssh {
                // Not implemented as a project picker at this time
                connection_string: "".into(),
                nickname: None,
            },
            #[cfg(any(test, feature = "test-support"))]
            RemoteConnectionOptions::Mock(options) => ProjectPickerData::Ssh {
                connection_string: format!("mock-{}", options.id).into(),
                nickname: None,
            },
        };
        let _path_task = cx
            .spawn_in(window, {
                let workspace = workspace;
                async move |this, cx| {
                    let Ok(Some(paths)) = rx.await else {
                        workspace
                            .update_in(cx, |workspace, window, cx| {
                                let fs = workspace.project().read(cx).fs().clone();
                                let weak = cx.entity().downgrade();
                                workspace.toggle_modal(window, cx, |window, cx| {
                                    RemoteServerProjects::new(
                                        create_new_window,
                                        fs,
                                        window,
                                        weak,
                                        cx,
                                    )
                                });
                            })
                            .log_err()?;
                        return None;
                    };

                    let app_state = workspace
                        .read_with(cx, |workspace, _| workspace.app_state().clone())
                        .ok()?;

                    let remote_connection = project.read_with(cx, |project, cx| {
                        project.remote_client()?.read(cx).connection()
                    })?;

                    let (paths, paths_with_positions) =
                        determine_paths_with_positions(&remote_connection, paths).await;

                    cx.update(|_, cx| {
                        let fs = app_state.fs.clone();
                        update_settings_file(fs, cx, {
                            let paths = paths
                                .iter()
                                .map(|path| path.to_string_lossy().into_owned())
                                .collect();
                            move |settings, _| match index {
                                ServerIndex::Ssh(index) => {
                                    if let Some(server) = settings
                                        .remote
                                        .ssh_connections
                                        .as_mut()
                                        .and_then(|connections| connections.get_mut(index.0))
                                    {
                                        server.projects.insert(RemoteProject { paths });
                                    };
                                }
                                ServerIndex::Wsl(index) => {
                                    if let Some(server) = settings
                                        .remote
                                        .wsl_connections
                                        .as_mut()
                                        .and_then(|connections| connections.get_mut(index.0))
                                    {
                                        server.projects.insert(RemoteProject { paths });
                                    };
                                }
                            }
                        });
                    })
                    .log_err();

                    let window = if create_new_window {
                        let options = cx
                            .update(|_, cx| (app_state.build_window_options)(None, cx))
                            .log_err()?;
                        cx.open_window(options, |window, cx| {
                            let workspace = cx.new(|cx| {
                                telemetry::event!("SSH Project Created");
                                Workspace::new(None, project.clone(), app_state.clone(), window, cx)
                            });
                            cx.new(|cx| MultiWorkspace::new(workspace, window, cx))
                        })
                        .log_err()
                    } else {
                        cx.window_handle().downcast::<MultiWorkspace>()
                    }?;

                    let items = open_remote_project_with_existing_connection(
                        connection, project, paths, app_state, window, None, None, cx,
                    )
                    .await
                    .log_err();

                    if let Some(items) = items {
                        for (item, path) in items.into_iter().zip(paths_with_positions) {
                            let Some(item) = item else {
                                continue;
                            };
                            let Some(row) = path.row else {
                                continue;
                            };
                            if let Some(active_editor) = item.downcast::<Editor>() {
                                window
                                    .update(cx, |_, window, cx| {
                                        active_editor.update(cx, |editor, cx| {
                                            let row = row.saturating_sub(1);
                                            let col = path.column.unwrap_or(0).saturating_sub(1);
                                            let Some(buffer) =
                                                editor.buffer().read(cx).as_singleton()
                                            else {
                                                return;
                                            };
                                            let buffer_snapshot = buffer.read(cx).snapshot();
                                            let point =
                                                buffer_snapshot.point_from_external_input(row, col);
                                            editor.go_to_singleton_buffer_point(point, window, cx);
                                        });
                                    })
                                    .ok();
                            }
                        }
                    }

                    this.update(cx, |_, cx| {
                        cx.emit(DismissEvent);
                    })
                    .ok();
                    Some(())
                }
            })
            .shared();
        cx.new(|_| Self {
            _path_task,
            picker,
            data,
        })
    }
}

impl gpui::Render for ProjectPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .child(match &self.data {
                ProjectPickerData::Ssh {
                    connection_string,
                    nickname,
                } => SshConnectionHeader {
                    connection_string: connection_string.clone(),
                    paths: Default::default(),
                    nickname: nickname.clone(),
                    is_wsl: false,
                    is_devcontainer: false,
                }
                .render(window, cx),
                ProjectPickerData::Wsl { distro_name } => SshConnectionHeader {
                    connection_string: distro_name.clone(),
                    paths: Default::default(),
                    nickname: None,
                    is_wsl: true,
                    is_devcontainer: false,
                }
                .render(window, cx),
            })
            .child(
                div()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.picker.clone()),
            )
    }
}

#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
struct SshServerIndex(usize);
impl std::fmt::Display for SshServerIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
struct WslServerIndex(usize);
impl std::fmt::Display for WslServerIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
enum ServerIndex {
    Ssh(SshServerIndex),
    Wsl(WslServerIndex),
}
impl From<SshServerIndex> for ServerIndex {
    fn from(index: SshServerIndex) -> Self {
        Self::Ssh(index)
    }
}
impl From<WslServerIndex> for ServerIndex {
    fn from(index: WslServerIndex) -> Self {
        Self::Wsl(index)
    }
}

#[derive(Clone)]
struct ProjectEntry {
    project: RemoteProject,
}

#[derive(Clone)]
enum RemoteEntry {
    Project {
        projects: Vec<ProjectEntry>,
        connection: Connection,
        index: ServerIndex,
    },
    SshConfig {
        host: SharedString,
    },
}

impl RemoteEntry {
    fn display_host(&self) -> &str {
        match self {
            Self::Project { connection, .. } => match connection {
                Connection::Ssh(c) => c.nickname.as_deref().unwrap_or(&c.host),
                Connection::Wsl(c) => &c.distro_name,
                Connection::DevContainer(c) => &c.name,
            },
            Self::SshConfig { host, .. } => host,
        }
    }

    /// Extra text to match against that isn't shown in the primary label.
    /// When an SSH connection has a nickname, [`display_host`] surfaces the
    /// nickname and the real host is only shown as a muted aux label, so we
    /// index the host here to keep it searchable.
    fn host_alias(&self) -> Option<&str> {
        match self {
            Self::Project {
                connection: Connection::Ssh(c),
                ..
            } if c.nickname.is_some() => Some(&c.host),
            _ => None,
        }
    }

    fn connection(&self) -> Cow<'_, Connection> {
        match self {
            Self::Project { connection, .. } => Cow::Borrowed(connection),
            Self::SshConfig { host, .. } => Cow::Owned(
                SshConnection {
                    host: host.to_string(),
                    ..SshConnection::default()
                }
                .into(),
            ),
        }
    }
}

#[derive(Clone)]
struct DefaultState {
    servers: Vec<RemoteEntry>,
    /// `None` when no filter is active; `Some` carries the fuzzy match results
    /// (server/project indices plus highlight positions) sorted by score.
    filtered_servers: Option<Vec<FilteredServer>>,
    filter_data: Arc<FilterData>,
}

impl DefaultState {
    fn new(ssh_config_servers: &BTreeSet<SharedString>, cx: &mut App) -> Self {
        let ssh_settings = RemoteSettings::get_global(cx);
        let read_ssh_config = ssh_settings.read_ssh_config;

        let ssh_servers = ssh_settings
            .ssh_connections()
            .enumerate()
            .map(|(index, connection)| {
                let projects = connection
                    .projects
                    .iter()
                    .map(|project| ProjectEntry {
                        project: project.clone(),
                    })
                    .collect();
                RemoteEntry::Project {
                    projects,
                    index: ServerIndex::Ssh(SshServerIndex(index)),
                    connection: connection.into(),
                }
            });

        let wsl_servers = ssh_settings
            .wsl_connections()
            .enumerate()
            .map(|(index, connection)| {
                let projects = connection
                    .projects
                    .iter()
                    .map(|project| ProjectEntry {
                        project: project.clone(),
                    })
                    .collect();
                RemoteEntry::Project {
                    projects,
                    index: ServerIndex::Wsl(WslServerIndex(index)),
                    connection: connection.into(),
                }
            });

        let mut servers = ssh_servers.chain(wsl_servers).collect::<Vec<RemoteEntry>>();

        if read_ssh_config {
            let mut extra_servers_from_config = ssh_config_servers.clone();
            for server in &servers {
                if let RemoteEntry::Project {
                    connection: Connection::Ssh(ssh_options),
                    ..
                } = server
                {
                    extra_servers_from_config.remove(&SharedString::new(ssh_options.host.clone()));
                }
            }
            servers.extend(
                extra_servers_from_config
                    .into_iter()
                    .map(|host| RemoteEntry::SshConfig { host }),
            );
        }

        let filter_data = Arc::new(FilterData::build(&servers));
        Self {
            servers,
            filtered_servers: None,
            filter_data,
        }
    }

    fn filter_sync(&mut self, query: &str) {
        if query.is_empty() {
            self.filtered_servers = None;
            return;
        }
        self.filtered_servers = Some(filter::run_sync(&self.filter_data, query));
    }
}

#[derive(Clone)]
enum ViewServerOptionsState {
    Ssh {
        connection: SshConnectionOptions,
        server_index: SshServerIndex,
        entries: [NavigableEntry; 4],
    },
    Wsl {
        connection: WslConnectionOptions,
        server_index: WslServerIndex,
        entries: [NavigableEntry; 2],
    },
}

impl ViewServerOptionsState {
    fn entries(&self) -> &[NavigableEntry] {
        match self {
            Self::Ssh { entries, .. } => entries,
            Self::Wsl { entries, .. } => entries,
        }
    }
}

enum Mode {
    Default,
    ViewServerOptions(ViewServerOptionsState),
    EditNickname(EditNicknameState),
    ProjectPicker(Entity<ProjectPicker>),
    CreateRemoteServer(CreateRemoteServer),
    CreateRemoteDevContainer(CreateRemoteDevContainer),
    #[cfg(target_os = "windows")]
    AddWslDistro(AddWslDistro),
}

impl Mode {
    /// The default mode is backed by [`RemoteServerProjects::default_picker`],
    /// which is rebuilt from settings independently, so this just selects the
    /// variant and ignores its arguments.
    fn default_mode(_ssh_config_servers: &BTreeSet<SharedString>, _cx: &mut App) -> Self {
        Self::Default
    }
}

enum RemoteMatch {
    AddServer,
    AddDevContainer,
    AddWsl,
    Separator,
    ServerHeader {
        server: usize,
        host_positions: Vec<usize>,
    },
    Project {
        server: usize,
        project: usize,
        positions: Vec<usize>,
    },
    OpenFolder {
        server: usize,
    },
    ViewServerOptions {
        server: usize,
    },
}

impl RemoteMatch {
    fn is_selectable(&self) -> bool {
        !matches!(
            self,
            RemoteMatch::Separator | RemoteMatch::ServerHeader { .. }
        )
    }
}

struct RemoteServerPickerDelegate {
    remote_server_projects: WeakEntity<RemoteServerProjects>,
    state: DefaultState,
    matches: Vec<RemoteMatch>,
    selected_index: usize,
    query: String,
    has_open_project: bool,
    is_local: bool,
}

impl RemoteServerPickerDelegate {
    fn new(
        remote_server_projects: WeakEntity<RemoteServerProjects>,
        ssh_config_servers: &BTreeSet<SharedString>,
        has_open_project: bool,
        is_local: bool,
        cx: &mut App,
    ) -> Self {
        let mut this = Self {
            remote_server_projects,
            state: DefaultState::new(ssh_config_servers, cx),
            matches: Vec::new(),
            selected_index: 0,
            query: String::new(),
            has_open_project,
            is_local,
        };
        this.rebuild_matches();
        this
    }

    fn reload(
        &mut self,
        ssh_config_servers: &BTreeSet<SharedString>,
        has_open_project: bool,
        is_local: bool,
        cx: &mut App,
    ) {
        self.has_open_project = has_open_project;
        self.is_local = is_local;
        self.state = DefaultState::new(ssh_config_servers, cx);
        // Settings/ssh-config changes are rare, so re-applying the active query
        // synchronously here is fine; the per-keystroke path filters off-thread.
        self.state.filter_sync(self.query.trim());
        self.rebuild_matches();
    }

    /// Flattens the current (already-filtered) `DefaultState` into the picker's
    /// match list. The fuzzy filtering itself runs separately (off-thread on the
    /// keystroke path, see [`Self::update_matches`]); this only reads
    /// [`DefaultState::filtered_servers`].
    fn rebuild_matches(&mut self) {
        let has_open_project = self.has_open_project;
        let is_local = self.is_local;

        let mut matches = Vec::new();
        if self.query.trim().is_empty() {
            matches.push(RemoteMatch::AddServer);
            if has_open_project && is_local {
                matches.push(RemoteMatch::AddDevContainer);
            }
            if cfg!(target_os = "windows") {
                matches.push(RemoteMatch::AddWsl);
            }
        }

        let push_server = |matches: &mut Vec<RemoteMatch>,
                           server_index: usize,
                           server: &RemoteEntry,
                           host_positions: Vec<usize>,
                           project_matches: Vec<(usize, Vec<usize>)>| {
            if !matches.is_empty() {
                matches.push(RemoteMatch::Separator);
            }
            matches.push(RemoteMatch::ServerHeader {
                server: server_index,
                host_positions,
            });
            match server {
                RemoteEntry::Project { .. } => {
                    for (project, positions) in project_matches {
                        matches.push(RemoteMatch::Project {
                            server: server_index,
                            project,
                            positions,
                        });
                    }
                    matches.push(RemoteMatch::OpenFolder {
                        server: server_index,
                    });
                    matches.push(RemoteMatch::ViewServerOptions {
                        server: server_index,
                    });
                }
                RemoteEntry::SshConfig { .. } => {
                    matches.push(RemoteMatch::OpenFolder {
                        server: server_index,
                    });
                }
            }
        };

        match &self.state.filtered_servers {
            None => {
                for (server_index, server) in self.state.servers.iter().enumerate() {
                    let project_matches = match server {
                        RemoteEntry::Project { projects, .. } => {
                            (0..projects.len()).map(|p| (p, Vec::new())).collect()
                        }
                        RemoteEntry::SshConfig { .. } => Vec::new(),
                    };
                    push_server(
                        &mut matches,
                        server_index,
                        server,
                        Vec::new(),
                        project_matches,
                    );
                }
            }
            Some(results) => {
                for filtered in results {
                    let server_index = filtered.server_index;
                    let Some(server) = self.state.servers.get(server_index) else {
                        continue;
                    };
                    let project_matches = filtered
                        .project_matches
                        .iter()
                        .map(|pm| (pm.project_index, pm.path_positions.clone()))
                        .collect();
                    push_server(
                        &mut matches,
                        server_index,
                        server,
                        filtered.host_positions.clone(),
                        project_matches,
                    );
                }
            }
        }

        self.matches = matches;
        self.selected_index = self
            .matches
            .iter()
            .position(RemoteMatch::is_selectable)
            .unwrap_or(0);
    }

    fn render_server_header(
        &self,
        server_index: usize,
        host_positions: &[usize],
    ) -> Option<AnyElement> {
        let server = self.state.servers.get(server_index)?;
        let connection = server.connection().into_owned();
        let (main_label, aux_label, is_wsl) = match &connection {
            Connection::Ssh(connection) => {
                if let Some(nickname) = connection.nickname.clone() {
                    let aux_label = SharedString::from(format!("({})", connection.host));
                    (nickname, Some(aux_label), false)
                } else {
                    (connection.host.clone(), None, false)
                }
            }
            Connection::Wsl(connection) => (connection.distro_name.clone(), None, true),
            Connection::DevContainer(connection) => (connection.name.clone(), None, false),
        };
        Some(
            h_flex()
                .w_full()
                .pt_1()
                .px_3()
                .gap_1()
                .overflow_hidden()
                .child(
                    h_flex()
                        .gap_1()
                        .max_w_96()
                        .overflow_hidden()
                        .text_ellipsis()
                        .when(is_wsl, |this| {
                            this.child(
                                Label::new("WSL:")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        })
                        .child(
                            HighlightedLabel::new(main_label, host_positions.to_vec())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
                .children(
                    aux_label
                        .map(|label| Label::new(label).size(LabelSize::Small).color(Color::Muted)),
                )
                .into_any_element(),
        )
    }

    fn render_action_item(
        &self,
        ix: usize,
        icon: IconName,
        label: &'static str,
        selected: bool,
    ) -> AnyElement {
        ListItem::new(("remote-action", ix))
            .toggle_state(selected)
            .inset(true)
            .spacing(ui::ListItemSpacing::Sparse)
            .start_slot(Icon::new(icon).color(Color::Muted))
            .child(Label::new(label))
            .into_any_element()
    }
}

impl PickerDelegate for RemoteServerPickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "RemoteServerPicker"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn can_select(&self, ix: usize, _window: &mut Window, _cx: &mut Context<Picker<Self>>) -> bool {
        self.matches.get(ix).is_some_and(RemoteMatch::is_selectable)
    }

    fn editor_position(&self) -> PickerEditorPosition {
        PickerEditorPosition::Start
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search remote projects…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No matching remote projects.".into())
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query;
        let query = self.query.trim().to_string();

        if query.is_empty() {
            self.state.filtered_servers = None;
            self.rebuild_matches();
            cx.notify();
            return Task::ready(());
        }

        let filter_data = self.state.filter_data.clone();
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |picker, cx| {
            // A fresh, never-set cancel flag: stale runs are abandoned when the
            // Picker drops this task on the next keystroke, so out-of-order
            // results can't be applied (mirrors `command_palette`).
            let cancel = AtomicBool::new(false);
            let Some(results) = filter::run_async(&filter_data, &query, &cancel, executor).await
            else {
                return;
            };
            picker
                .update(cx, |picker, cx| {
                    picker.delegate.state.filtered_servers = Some(results);
                    picker.delegate.rebuild_matches();
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self.matches.get(self.selected_index) else {
            return;
        };
        let remote_server_projects = self.remote_server_projects.clone();
        match entry {
            RemoteMatch::Separator | RemoteMatch::ServerHeader { .. } => {}
            RemoteMatch::AddServer => {
                remote_server_projects
                    .update(cx, |this, cx| {
                        this.mode = Mode::CreateRemoteServer(CreateRemoteServer::new(window, cx));
                        cx.notify();
                    })
                    .ok();
            }
            RemoteMatch::AddDevContainer => {
                remote_server_projects
                    .update(cx, |this, cx| {
                        this.init_dev_container_mode(window, cx);
                    })
                    .ok();
            }
            RemoteMatch::AddWsl => {
                #[cfg(target_os = "windows")]
                remote_server_projects
                    .update(cx, |this, cx| {
                        this.mode = Mode::AddWslDistro(AddWslDistro::new(window, cx));
                        cx.notify();
                    })
                    .ok();
            }
            RemoteMatch::Project {
                server, project, ..
            } => {
                let Some(RemoteEntry::Project {
                    connection,
                    index,
                    projects,
                    ..
                }) = self.state.servers.get(*server)
                else {
                    return;
                };
                let Some(project_entry) = projects.get(*project) else {
                    return;
                };
                let connection = connection.clone();
                let index = *index;
                let project = project_entry.project.clone();
                remote_server_projects
                    .update(cx, |this, cx| {
                        this.open_remote_project_entry(
                            index, project, connection, secondary, window, cx,
                        );
                    })
                    .ok();
            }
            RemoteMatch::OpenFolder { server } => {
                let Some(server_entry) = self.state.servers.get(*server) else {
                    return;
                };
                match server_entry {
                    RemoteEntry::Project {
                        connection, index, ..
                    } => {
                        let connection = connection.clone();
                        let index = *index;
                        remote_server_projects
                            .update(cx, |this, cx| {
                                this.create_remote_project(
                                    index,
                                    connection.into(),
                                    None,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    }
                    RemoteEntry::SshConfig { host, .. } => {
                        let host = host.clone();
                        let connection = server_entry.connection().into_owned();
                        remote_server_projects
                            .update(cx, |this, cx| {
                                let new_ix = this.create_host_from_ssh_config(&host, cx);
                                this.create_remote_project(
                                    new_ix.into(),
                                    connection.into(),
                                    None,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    }
                }
            }
            RemoteMatch::ViewServerOptions { server } => {
                let Some(RemoteEntry::Project {
                    connection, index, ..
                }) = self.state.servers.get(*server)
                else {
                    return;
                };
                let connection = connection.clone();
                let index = *index;
                remote_server_projects
                    .update(cx, |this, cx| {
                        this.view_server_options((index, connection.into()), window, cx);
                    })
                    .ok();
            }
        }
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry = self.matches.get(ix)?;
        match entry {
            RemoteMatch::Separator => Some(div().child(ListSeparator).into_any_element()),
            RemoteMatch::ServerHeader {
                server,
                host_positions,
            } => self.render_server_header(*server, host_positions),
            RemoteMatch::AddServer => {
                Some(self.render_action_item(ix, IconName::Plus, "Connect SSH Server", selected))
            }
            RemoteMatch::AddDevContainer => {
                Some(self.render_action_item(ix, IconName::Plus, "Connect Dev Container", selected))
            }
            RemoteMatch::AddWsl => {
                Some(self.render_action_item(ix, IconName::Plus, "Add WSL Distro", selected))
            }
            RemoteMatch::OpenFolder { .. } => {
                Some(self.render_action_item(ix, IconName::Plus, "Open Folder", selected))
            }
            RemoteMatch::ViewServerOptions { .. } => Some(self.render_action_item(
                ix,
                IconName::Settings,
                "View Server Options",
                selected,
            )),
            RemoteMatch::Project {
                server,
                project,
                positions,
            } => {
                let server_entry = self.state.servers.get(*server)?;
                let RemoteEntry::Project {
                    projects, index, ..
                } = server_entry
                else {
                    return None;
                };
                let project_entry = projects.get(*project)?;
                let server_ix = *index;
                let remote_project = project_entry.project.clone();
                let paths = remote_project.paths.clone();
                let remote_server_projects = self.remote_server_projects.clone();

                Some(
                    ListItem::new(("remote-project", ix))
                        .toggle_state(selected)
                        .inset(true)
                        .spacing(ui::ListItemSpacing::Sparse)
                        .start_slot(
                            Icon::new(IconName::Folder)
                                .color(Color::Muted)
                                .size(IconSize::Small),
                        )
                        .child(
                            HighlightedLabel::new(paths.join(", "), positions.clone())
                                .truncate_start(),
                        )
                        .tooltip(Tooltip::text(paths.join("\n")))
                        .end_slot(
                            div().mr_2().child(
                                IconButton::new("remove-remote-project", IconName::Trash)
                                    .icon_size(IconSize::Small)
                                    .shape(IconButtonShape::Square)
                                    .size(ButtonSize::Large)
                                    .tooltip(Tooltip::text("Delete Remote Project"))
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        let remote_project = remote_project.clone();
                                        remote_server_projects
                                            .update(cx, |this, cx| {
                                                this.delete_remote_project(
                                                    server_ix,
                                                    &remote_project,
                                                    cx,
                                                );
                                            })
                                            .ok();
                                    })),
                            ),
                        )
                        .show_end_slot_on_hover()
                        .into_any_element(),
                )
            }
        }
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let is_project_selected = matches!(
            self.matches.get(self.selected_index),
            Some(RemoteMatch::Project { .. })
        );

        let confirm_button = |label: SharedString| {
            Button::new("select", label)
                .key_binding(KeyBinding::for_action(&menu::Confirm, cx))
                .on_click(|_, window, cx| window.dispatch_action(menu::Confirm.boxed_clone(), cx))
        };

        let buttons = if is_project_selected {
            h_flex()
                .gap_1()
                .child(
                    Button::new("open_new_window", "New Window")
                        .key_binding(KeyBinding::for_action(&menu::SecondaryConfirm, cx))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(menu::SecondaryConfirm.boxed_clone(), cx)
                        }),
                )
                .child(confirm_button("Open".into()))
                .into_any_element()
        } else {
            confirm_button("Select".into()).into_any_element()
        };

        Some(
            h_flex()
                .w_full()
                .p_1p5()
                .justify_end()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(buttons)
                .into_any(),
        )
    }
}

impl RemoteServerProjects {
    #[cfg(target_os = "windows")]
    pub fn wsl(
        create_new_window: bool,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_inner(
            Mode::AddWslDistro(AddWslDistro::new(window, cx)),
            create_new_window,
            fs,
            window,
            workspace,
            cx,
        )
    }

    pub fn new(
        create_new_window: bool,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_inner(
            Mode::default_mode(&BTreeSet::new(), cx),
            create_new_window,
            fs,
            window,
            workspace,
            cx,
        )
    }

    /// Creates a new RemoteServerProjects modal that opens directly in dev container creation mode.
    /// Used when suggesting dev container connection from toast notification.
    pub fn new_dev_container(
        fs: Arc<dyn Fs>,
        configs: Vec<DevContainerConfig>,
        app_state: Arc<AppState>,
        dev_container_context: Option<DevContainerContext>,
        window: &mut Window,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial_mode = if configs.len() > 1 {
            DevContainerCreationProgress::SelectingConfig
        } else {
            DevContainerCreationProgress::Creating
        };

        let mut this = Self::new_inner(
            Mode::CreateRemoteDevContainer(CreateRemoteDevContainer::new(initial_mode, cx)),
            false,
            fs,
            window,
            workspace,
            cx,
        );

        if configs.len() > 1 {
            let delegate = DevContainerPickerDelegate::new(configs, cx.weak_entity());
            this.dev_container_picker =
                Some(cx.new(|cx| Picker::uniform_list(delegate, window, cx).embedded()));
        } else if let Some(context) = dev_container_context {
            let config = configs.into_iter().next();
            this.open_dev_container(config, app_state, context, window, cx);
            this.view_in_progress_dev_container(window, cx);
        } else {
            log::error!("No active project directory for Dev Container");
        }

        this
    }

    pub fn popover(
        fs: Arc<dyn Fs>,
        workspace: WeakEntity<Workspace>,
        create_new_window: Option<bool>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let create_new_window =
            create_new_window.unwrap_or_else(|| crate::default_open_in_new_window(cx));
        cx.new(|cx| {
            let server = Self::new(create_new_window, fs, window, workspace, cx);
            server.focus_handle(cx).focus(window, cx);
            server
        })
    }

    fn new_inner(
        mode: Mode,
        create_new_window: bool,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let remote_server_projects = cx.weak_entity();
        // The modal is constructed inside a `workspace.update`, so the workspace
        // entity can't be read here; start with conservative defaults and refresh
        // the real flags via `defer_in` once construction completes.
        let default_picker = cx.new(|cx| {
            let delegate = RemoteServerPickerDelegate::new(
                remote_server_projects,
                &BTreeSet::new(),
                false,
                true,
                cx,
            );
            Picker::list(delegate, window, cx).embedded()
        });
        let mut read_ssh_config = RemoteSettings::get_global(cx).read_ssh_config;
        let ssh_config_updates = if read_ssh_config {
            spawn_ssh_config_watch(fs.clone(), cx)
        } else {
            Task::ready(())
        };

        let settings_subscription =
            cx.observe_global_in::<SettingsStore>(window, move |recent_projects, window, cx| {
                let new_read_ssh_config = RemoteSettings::get_global(cx).read_ssh_config;
                if read_ssh_config != new_read_ssh_config {
                    read_ssh_config = new_read_ssh_config;
                    if read_ssh_config {
                        recent_projects.ssh_config_updates = spawn_ssh_config_watch(fs.clone(), cx);
                    } else {
                        recent_projects.ssh_config_servers.clear();
                        recent_projects.ssh_config_updates = Task::ready(());
                    }
                }
                recent_projects.refresh_default_picker(window, cx);
            });

        let dismiss_subscription = cx.subscribe(&default_picker, |_, _, _, cx| {
            cx.emit(DismissEvent);
        });

        cx.defer_in(window, |this, window, cx| {
            this.refresh_default_picker(window, cx);
        });

        Self {
            mode,
            focus_handle,
            default_picker,
            workspace,
            #[cfg(target_os = "windows")]
            retained_connections: Vec::new(),
            ssh_config_updates,
            ssh_config_servers: BTreeSet::new(),
            create_new_window,
            dev_container_picker: None,
            _subscriptions: vec![settings_subscription, dismiss_subscription],
            allow_dismissal: true,
        }
    }

    fn project_picker(
        create_new_window: bool,
        index: ServerIndex,
        connection_options: remote::RemoteConnectionOptions,
        project: Entity<Project>,
        home_dir: RemotePathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
        workspace: WeakEntity<Workspace>,
    ) -> Self {
        let fs = project.read(cx).fs().clone();
        let mut this = Self::new(create_new_window, fs, window, workspace.clone(), cx);
        this.mode = Mode::ProjectPicker(ProjectPicker::new(
            create_new_window,
            index,
            connection_options,
            project,
            home_dir,
            workspace,
            window,
            cx,
        ));
        cx.notify();

        this
    }

    fn create_ssh_server(
        &mut self,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = get_text(&editor, cx);
        if input.is_empty() {
            return;
        }

        let connection_options = match SshConnectionOptions::parse_command_line(&input) {
            Ok(c) => c,
            Err(e) => {
                self.mode = Mode::CreateRemoteServer(CreateRemoteServer {
                    address_editor: editor,
                    address_error: Some(format!("could not parse: {:?}", e).into()),
                    ssh_prompt: None,
                    _creating: None,
                });
                return;
            }
        };
        let ssh_prompt = cx.new(|cx| {
            RemoteConnectionPrompt::new(
                connection_options.connection_string(),
                connection_options.nickname.clone(),
                false,
                false,
                window,
                cx,
            )
        });

        let connection = connect(
            ConnectionIdentifier::setup(),
            RemoteConnectionOptions::Ssh(connection_options.clone()),
            ssh_prompt.clone(),
            window,
            cx,
        )
        .prompt_err("Failed to connect", window, cx, |_, _, _| None);

        let address_editor = editor.clone();
        let creating = cx.spawn_in(window, async move |this, cx| {
            match connection.await {
                Some(Some(client)) => this
                    .update_in(cx, |this, window, cx| {
                        info!("ssh server created");
                        telemetry::event!("SSH Server Created");
                        this.finish_create_ssh_server(connection_options, client, window, cx);
                    })
                    .log_err(),
                _ => this
                    .update(cx, |this, cx| {
                        address_editor.update(cx, |this, _| {
                            this.set_read_only(false);
                        });
                        this.mode = Mode::CreateRemoteServer(CreateRemoteServer {
                            address_editor,
                            address_error: None,
                            ssh_prompt: None,
                            _creating: None,
                        });
                        cx.notify()
                    })
                    .log_err(),
            };
            None
        });

        editor.update(cx, |this, _| {
            this.set_read_only(true);
        });
        self.mode = Mode::CreateRemoteServer(CreateRemoteServer {
            address_editor: editor,
            address_error: None,
            ssh_prompt: Some(ssh_prompt),
            _creating: Some(creating),
        });
    }

    fn finish_create_ssh_server(
        &mut self,
        connection_options: SshConnectionOptions,
        client: Entity<RemoteClient>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let index = SshServerIndex(RemoteSettings::get_global(cx).ssh_connections().count());
        self.add_ssh_server(connection_options.clone(), cx);
        self.create_remote_project(
            index.into(),
            connection_options.into(),
            Some(client),
            window,
            cx,
        );
    }

    #[cfg(target_os = "windows")]
    fn connect_wsl_distro(
        &mut self,
        picker: Entity<Picker<crate::wsl_picker::WslPickerDelegate>>,
        distro: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let connection_options = WslConnectionOptions {
            distro_name: distro,
            user: None,
        };

        let prompt = cx.new(|cx| {
            RemoteConnectionPrompt::new(
                connection_options.distro_name.clone(),
                None,
                true,
                false,
                window,
                cx,
            )
        });
        let connection = connect(
            ConnectionIdentifier::setup(),
            connection_options.clone().into(),
            prompt.clone(),
            window,
            cx,
        )
        .prompt_err("Failed to connect", window, cx, |_, _, _| None);

        let wsl_picker = picker.clone();
        let creating = cx.spawn_in(window, async move |this, cx| {
            match connection.await {
                Some(Some(client)) => this.update_in(cx, |this, window, cx| {
                    telemetry::event!("WSL Distro Added");
                    this.retained_connections.push(client);
                    let Some(fs) = this
                        .workspace
                        .read_with(cx, |workspace, cx| {
                            workspace.project().read(cx).fs().clone()
                        })
                        .log_err()
                    else {
                        return;
                    };

                    crate::add_wsl_distro(fs, &connection_options, cx);
                    this.mode = Mode::default_mode(&BTreeSet::new(), cx);
                    this.focus_handle(cx).focus(window, cx);
                    cx.notify();
                }),
                _ => this.update(cx, |this, cx| {
                    this.mode = Mode::AddWslDistro(AddWslDistro {
                        picker: wsl_picker,
                        connection_prompt: None,
                        _creating: None,
                    });
                    cx.notify();
                }),
            }
            .log_err();
        });

        self.mode = Mode::AddWslDistro(AddWslDistro {
            picker,
            connection_prompt: Some(prompt),
            _creating: Some(creating),
        });
    }

    fn view_server_options(
        &mut self,
        (server_index, connection): (ServerIndex, RemoteConnectionOptions),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mode = Mode::ViewServerOptions(match (server_index, connection) {
            (ServerIndex::Ssh(server_index), RemoteConnectionOptions::Ssh(connection)) => {
                ViewServerOptionsState::Ssh {
                    connection,
                    server_index,
                    entries: std::array::from_fn(|_| NavigableEntry::focusable(cx)),
                }
            }
            (ServerIndex::Wsl(server_index), RemoteConnectionOptions::Wsl(connection)) => {
                ViewServerOptionsState::Wsl {
                    connection,
                    server_index,
                    entries: std::array::from_fn(|_| NavigableEntry::focusable(cx)),
                }
            }
            _ => {
                log::error!("server index and connection options mismatch");
                self.mode = Mode::default_mode(&BTreeSet::default(), cx);
                return;
            }
        });
        self.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn view_in_progress_dev_container(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.allow_dismissal = false;
        self.mode = Mode::CreateRemoteDevContainer(CreateRemoteDevContainer::new(
            DevContainerCreationProgress::Creating,
            cx,
        ));
        self.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn create_remote_project(
        &mut self,
        index: ServerIndex,
        connection_options: RemoteConnectionOptions,
        session: Option<Entity<RemoteClient>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        let create_new_window = self.create_new_window;
        workspace.update(cx, |_, cx| {
            cx.defer_in(window, move |workspace, window, cx| {
                let app_state = workspace.app_state().clone();
                workspace.toggle_modal(window, cx, |window, cx| {
                    RemoteConnectionModal::new(&connection_options, Vec::new(), window, cx)
                });
                // can be None if another copy of this modal opened in the meantime
                let Some(modal) = workspace.active_modal::<RemoteConnectionModal>(cx) else {
                    return;
                };
                let prompt = modal.read(cx).prompt.clone();

                let connect = match session {
                    Some(session) => Task::ready(Ok(Some(session))),
                    None => connect(
                        ConnectionIdentifier::setup(),
                        connection_options.clone(),
                        prompt,
                        window,
                        cx,
                    ),
                }
                .prompt_err("Failed to connect", window, cx, |_, _, _| None);

                cx.spawn_in(window, async move |workspace, cx| {
                    let session = connect.await;

                    workspace.update(cx, |workspace, cx| {
                        if let Some(prompt) = workspace.active_modal::<RemoteConnectionModal>(cx) {
                            prompt.update(cx, |prompt, cx| prompt.finished(cx))
                        }
                    })?;

                    let Some(Some(session)) = session else {
                        return workspace.update_in(cx, |workspace, window, cx| {
                            let weak = cx.entity().downgrade();
                            let fs = workspace.project().read(cx).fs().clone();
                            workspace.toggle_modal(window, cx, |window, cx| {
                                RemoteServerProjects::new(create_new_window, fs, window, weak, cx)
                            });
                        });
                    };

                    let (path_style, project) = cx.update(|_, cx| {
                        (
                            session.read(cx).path_style(),
                            project::Project::remote(
                                session,
                                app_state.client.clone(),
                                app_state.node_runtime.clone(),
                                app_state.user_store.clone(),
                                app_state.languages.clone(),
                                app_state.fs.clone(),
                                true,
                                cx,
                            ),
                        )
                    })?;

                    let home_dir = project
                        .read_with(cx, |project, cx| project.resolve_abs_path("~", cx))
                        .await
                        .and_then(|path| path.into_abs_path())
                        .map(|path| RemotePathBuf::new(path, path_style))
                        .unwrap_or_else(|| match path_style {
                            PathStyle::Unix => RemotePathBuf::from_str("/", PathStyle::Unix),
                            PathStyle::Windows => {
                                RemotePathBuf::from_str("C:\\", PathStyle::Windows)
                            }
                        });

                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            let weak = cx.entity().downgrade();
                            workspace.toggle_modal(window, cx, |window, cx| {
                                RemoteServerProjects::project_picker(
                                    create_new_window,
                                    index,
                                    connection_options,
                                    project,
                                    home_dir,
                                    window,
                                    cx,
                                    weak,
                                )
                            });
                        })
                        .ok();
                    Ok(())
                })
                .detach();
            })
        })
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        match &self.mode {
            Mode::Default | Mode::ViewServerOptions(_) => {}
            Mode::ProjectPicker(_) => {}
            Mode::CreateRemoteServer(state) => {
                if let Some(prompt) = state.ssh_prompt.as_ref() {
                    prompt.update(cx, |prompt, cx| {
                        prompt.confirm(window, cx);
                    });
                    return;
                }

                self.create_ssh_server(state.address_editor.clone(), window, cx);
            }
            Mode::CreateRemoteDevContainer(_) => {}
            Mode::EditNickname(state) => {
                let text = Some(state.editor.read(cx).text(cx)).filter(|text| !text.is_empty());
                let index = state.index;
                self.update_settings_file(cx, move |setting, _| {
                    if let Some(connections) = setting.ssh_connections.as_mut()
                        && let Some(connection) = connections.get_mut(index.0)
                    {
                        connection.nickname = text;
                    }
                });
                self.mode = Mode::default_mode(&self.ssh_config_servers, cx);
                self.focus_handle.focus(window, cx);
            }
            #[cfg(target_os = "windows")]
            Mode::AddWslDistro(state) => {
                let delegate = &state.picker.read(cx).delegate;
                let distro = delegate.selected_distro().unwrap();
                self.connect_wsl_distro(state.picker.clone(), distro, window, cx);
            }
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        match &self.mode {
            Mode::Default => {
                cx.emit(DismissEvent);
            }
            Mode::CreateRemoteServer(state) if state.ssh_prompt.is_some() => {
                let new_state = CreateRemoteServer::new(window, cx);
                let old_prompt = state.address_editor.read(cx).text(cx);
                new_state.address_editor.update(cx, |this, cx| {
                    this.set_text(old_prompt, window, cx);
                });

                self.mode = Mode::CreateRemoteServer(new_state);
                cx.notify();
            }
            Mode::CreateRemoteDevContainer(CreateRemoteDevContainer {
                progress: DevContainerCreationProgress::Error(_),
                ..
            }) => {
                cx.emit(DismissEvent);
            }
            _ => {
                self.allow_dismissal = true;
                self.mode = Mode::default_mode(&self.ssh_config_servers, cx);
                self.focus_handle(cx).focus(window, cx);
                cx.notify();
            }
        }
    }

    /// Rebuilds the default picker's data from the latest settings/ssh-config
    /// and re-applies the current filter query.
    fn workspace_flags(workspace: &WeakEntity<Workspace>, cx: &App) -> (bool, bool) {
        let has_open_project = workspace
            .upgrade()
            .map(|workspace| {
                workspace
                    .read(cx)
                    .project()
                    .read(cx)
                    .visible_worktrees(cx)
                    .next()
                    .is_some()
            })
            .unwrap_or(false);
        let is_local = workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).project().read(cx).is_local())
            .unwrap_or(true);
        (has_open_project, is_local)
    }

    fn refresh_default_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ssh_config_servers = self.ssh_config_servers.clone();
        let (has_open_project, is_local) = Self::workspace_flags(&self.workspace, cx);
        self.default_picker.update(cx, |picker, cx| {
            picker
                .delegate
                .reload(&ssh_config_servers, has_open_project, is_local, cx);
            picker.refresh(window, cx);
        });
    }

    /// Opens a saved remote project, mirroring whether a new window should be
    /// created based on the modal's `create_new_window` preference and whether
    /// the confirm was a secondary (platform-modifier) confirm.
    fn open_remote_project_entry(
        &mut self,
        _index: ServerIndex,
        project: RemoteProject,
        connection: Connection,
        secondary_confirm: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(app_state) = self
            .workspace
            .read_with(cx, |workspace, _| workspace.app_state().clone())
            .log_err()
        else {
            return;
        };
        let create_new_window = self.create_new_window;
        cx.emit(DismissEvent);

        let replace_window = match (create_new_window, secondary_confirm) {
            (true, false) | (false, true) => None,
            (true, true) | (false, false) => window.window_handle().downcast::<MultiWorkspace>(),
        };

        cx.spawn_in(window, async move |_, cx| {
            let result = open_remote_project(
                connection.into(),
                project.paths.into_iter().map(PathBuf::from).collect(),
                app_state,
                OpenOptions {
                    requesting_window: replace_window,
                    ..OpenOptions::default()
                },
                cx,
            )
            .await;
            if let Err(e) = result {
                log::error!("Failed to connect: {e:#}");
                cx.prompt(
                    gpui::PromptLevel::Critical,
                    "Failed to connect",
                    Some(&e.to_string()),
                    &["OK"],
                )
                .await
                .ok();
            }
        })
        .detach();
    }

    fn update_settings_file(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut RemoteSettingsContent, &App) + Send + Sync + 'static,
    ) {
        let Some(fs) = self
            .workspace
            .read_with(cx, |workspace, _| workspace.app_state().fs.clone())
            .log_err()
        else {
            return;
        };
        update_settings_file(fs, cx, move |setting, cx| f(&mut setting.remote, cx));
    }

    fn delete_ssh_server(&mut self, server: SshServerIndex, cx: &mut Context<Self>) {
        self.update_settings_file(cx, move |setting, _| {
            if let Some(connections) = setting.ssh_connections.as_mut()
                && connections.get(server.0).is_some()
            {
                connections.remove(server.0);
            }
        });
    }

    fn delete_remote_project(
        &mut self,
        server: ServerIndex,
        project: &RemoteProject,
        cx: &mut Context<Self>,
    ) {
        match server {
            ServerIndex::Ssh(server) => {
                self.delete_ssh_project(server, project, cx);
            }
            ServerIndex::Wsl(server) => {
                self.delete_wsl_project(server, project, cx);
            }
        }
    }

    fn delete_ssh_project(
        &mut self,
        server: SshServerIndex,
        project: &RemoteProject,
        cx: &mut Context<Self>,
    ) {
        let project = project.clone();
        self.update_settings_file(cx, move |setting, _| {
            if let Some(server) = setting
                .ssh_connections
                .as_mut()
                .and_then(|connections| connections.get_mut(server.0))
            {
                server.projects.remove(&project);
            }
        });
    }

    fn delete_wsl_project(
        &mut self,
        server: WslServerIndex,
        project: &RemoteProject,
        cx: &mut Context<Self>,
    ) {
        let project = project.clone();
        self.update_settings_file(cx, move |setting, _| {
            if let Some(server) = setting
                .wsl_connections
                .as_mut()
                .and_then(|connections| connections.get_mut(server.0))
            {
                server.projects.remove(&project);
            }
        });
    }

    fn delete_wsl_distro(&mut self, server: WslServerIndex, cx: &mut Context<Self>) {
        self.update_settings_file(cx, move |setting, _| {
            if let Some(connections) = setting.wsl_connections.as_mut() {
                connections.remove(server.0);
            }
        });
    }

    fn add_ssh_server(
        &mut self,
        connection_options: remote::SshConnectionOptions,
        cx: &mut Context<Self>,
    ) {
        self.update_settings_file(cx, move |setting, _| {
            setting
                .ssh_connections
                .get_or_insert(Default::default())
                .push(SshConnection {
                    host: connection_options.host.to_string(),
                    username: connection_options.username,
                    port: connection_options.port,
                    projects: BTreeSet::new(),
                    nickname: None,
                    args: connection_options.args.unwrap_or_default(),
                    upload_binary_over_ssh: None,
                    port_forwards: connection_options.port_forwards,
                    connection_timeout: connection_options.connection_timeout,
                })
        });
    }

    fn edit_in_dev_container_json(
        &mut self,
        config: Option<DevContainerConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            cx.emit(DismissEvent);
            cx.notify();
            return;
        };

        let config_path = config
            .map(|c| c.config_path)
            .unwrap_or_else(|| PathBuf::from(".devcontainer/devcontainer.json"));

        workspace.update(cx, |workspace, cx| {
            let project = workspace.project().clone();

            let worktree = project
                .read(cx)
                .visible_worktrees(cx)
                .find_map(|tree| tree.read(cx).root_entry()?.is_dir().then_some(tree));

            if let Some(worktree) = worktree {
                let tree_id = worktree.read(cx).id();
                let devcontainer_path =
                    match RelPath::new(&config_path, util::paths::PathStyle::Unix) {
                        Ok(path) => path.into_owned(),
                        Err(error) => {
                            log::error!(
                                "Invalid devcontainer path: {} - {}",
                                config_path.display(),
                                error
                            );
                            return;
                        }
                    };
                cx.spawn_in(window, async move |workspace, cx| {
                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace.open_path(
                                (tree_id, devcontainer_path),
                                None,
                                true,
                                window,
                                cx,
                            )
                        })?
                        .await
                })
                .detach();
            } else {
                return;
            }
        });
        cx.emit(DismissEvent);
        cx.notify();
    }

    fn init_dev_container_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let configs = self
            .workspace
            .read_with(cx, |workspace, cx| find_devcontainer_configs(workspace, cx))
            .unwrap_or_default();

        if configs.len() > 1 {
            let delegate = DevContainerPickerDelegate::new(configs, cx.weak_entity());
            self.dev_container_picker =
                Some(cx.new(|cx| Picker::uniform_list(delegate, window, cx).embedded()));

            let state =
                CreateRemoteDevContainer::new(DevContainerCreationProgress::SelectingConfig, cx);
            self.mode = Mode::CreateRemoteDevContainer(state);
            cx.notify();
        } else if let Some((app_state, context)) = self
            .workspace
            .read_with(cx, |workspace, cx| {
                let app_state = workspace.app_state().clone();
                let context = DevContainerContext::from_workspace(workspace, cx)?;
                Some((app_state, context))
            })
            .ok()
            .flatten()
        {
            let config = configs.into_iter().next();
            self.open_dev_container(config, app_state, context, window, cx);
            self.view_in_progress_dev_container(window, cx);
        } else {
            log::error!("No active project directory for Dev Container");
        }
    }

    fn open_dev_container(
        &self,
        config: Option<DevContainerConfig>,
        app_state: Arc<AppState>,
        context: DevContainerContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let replace_window = window.window_handle().downcast::<MultiWorkspace>();
        let app_state = Arc::downgrade(&app_state);

        cx.spawn_in(window, async move |entity, cx| {
            let environment = context.environment(cx).await;

            let (dev_container_connection, starting_dir) =
                match start_dev_container_with_config(context, config, environment).await {
                    Ok((c, s)) => (c, s),
                    Err(e) => {
                        log::error!("Failed to start dev container: {:?}", e);
                        cx.prompt(
                            gpui::PromptLevel::Critical,
                            "Failed to start Dev Container. See logs for details",
                            Some(&format!("{e}")),
                            &["OK"],
                        )
                        .await
                        .ok();
                        entity
                            .update_in(cx, |remote_server_projects, window, cx| {
                                remote_server_projects.allow_dismissal = true;
                                remote_server_projects.mode =
                                    Mode::CreateRemoteDevContainer(CreateRemoteDevContainer::new(
                                        DevContainerCreationProgress::Error(format!("{e}")),
                                        cx,
                                    ));
                                remote_server_projects.focus_handle(cx).focus(window, cx);
                            })
                            .ok();
                        return;
                    }
                };
            cx.update(|_, cx| {
                ExtensionStore::global(cx).update(cx, |this, cx| {
                    for extension in &dev_container_connection.extension_ids {
                        log::info!("Installing extension {extension} from devcontainer");
                        this.install_latest_extension(Arc::from(extension.clone()), cx);
                    }
                })
            })
            .log_err();

            entity
                .update(cx, |this, cx| {
                    this.allow_dismissal = true;
                    cx.emit(DismissEvent);
                })
                .log_err();

            let Some(app_state) = app_state.upgrade() else {
                return;
            };
            let result = open_remote_project(
                Connection::DevContainer(dev_container_connection).into(),
                vec![starting_dir].into_iter().map(PathBuf::from).collect(),
                app_state,
                OpenOptions {
                    requesting_window: replace_window,
                    ..OpenOptions::default()
                },
                cx,
            )
            .await;
            if let Err(e) = result {
                log::error!("Failed to connect: {e:#}");
                cx.prompt(
                    gpui::PromptLevel::Critical,
                    "Failed to connect",
                    Some(&e.to_string()),
                    &["OK"],
                )
                .await
                .ok();
            }
        })
        .detach();
    }

    fn render_create_dev_container(
        &self,
        state: &CreateRemoteDevContainer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        match &state.progress {
            DevContainerCreationProgress::Error(message) => {
                let view = Navigable::new(
                    div()
                        .child(
                            div().track_focus(&self.focus_handle(cx)).size_full().child(
                                v_flex().py_1().child(
                                    ListItem::new("Error")
                                        .inset(true)
                                        .selectable(false)
                                        .spacing(ui::ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::XCircle).color(Color::Error),
                                        )
                                        .child(Label::new("Error Creating Dev Container:"))
                                        .child(Label::new(message).buffer_font(cx)),
                                ),
                            ),
                        )
                        .child(ListSeparator)
                        .child(
                            div()
                                .id("devcontainer-see-log")
                                .track_focus(&state.view_logs_entry.focus_handle)
                                .on_action(cx.listener(|_, _: &menu::Confirm, window, cx| {
                                    window.dispatch_action(Box::new(OpenLog), cx);
                                    cx.emit(DismissEvent);
                                    cx.notify();
                                }))
                                .child(
                                    ListItem::new("li-devcontainer-see-log")
                                        .toggle_state(
                                            state
                                                .view_logs_entry
                                                .focus_handle
                                                .contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ui::ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::File)
                                                .color(Color::Muted)
                                                .size(IconSize::Small),
                                        )
                                        .child(Label::new("Open Zed Log"))
                                        .on_click(cx.listener(|_, _, window, cx| {
                                            window.dispatch_action(Box::new(OpenLog), cx);
                                            cx.emit(DismissEvent);
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .id("devcontainer-go-back")
                                .track_focus(&state.back_entry.focus_handle)
                                .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                                    this.cancel(&menu::Cancel, window, cx);
                                    cx.notify();
                                }))
                                .child(
                                    ListItem::new("li-devcontainer-go-back")
                                        .toggle_state(
                                            state
                                                .back_entry
                                                .focus_handle
                                                .contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ui::ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::Exit)
                                                .color(Color::Muted)
                                                .size(IconSize::Small),
                                        )
                                        .child(Label::new("Exit"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.cancel(&menu::Cancel, window, cx);
                                            cx.notify();
                                        })),
                                ),
                        )
                        .into_any_element(),
                )
                .entry(state.view_logs_entry.clone())
                .entry(state.back_entry.clone());
                view.render(window, cx).into_any_element()
            }
            DevContainerCreationProgress::SelectingConfig => {
                self.render_config_selection(window, cx).into_any_element()
            }
            DevContainerCreationProgress::Creating => {
                self.focus_handle(cx).focus(window, cx);
                div()
                    .track_focus(&self.focus_handle(cx))
                    .size_full()
                    .child(
                        v_flex()
                            .pb_1()
                            .child(
                                ModalHeader::new().child(
                                    Headline::new("Dev Containers").size(HeadlineSize::XSmall),
                                ),
                            )
                            .child(ListSeparator)
                            .child(
                                ListItem::new("creating")
                                    .inset(true)
                                    .spacing(ui::ListItemSpacing::Sparse)
                                    .disabled(true)
                                    .start_slot(
                                        Icon::new(IconName::ArrowCircle)
                                            .color(Color::Muted)
                                            .with_rotate_animation(2),
                                    )
                                    .child(
                                        h_flex()
                                            .opacity(0.6)
                                            .gap_1()
                                            .child(Label::new("Creating Dev Container"))
                                            .child(LoadingLabel::new("")),
                                    ),
                            ),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_config_selection(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(picker) = &self.dev_container_picker else {
            return div().into_any_element();
        };

        let content = v_flex().pb_1().child(picker.clone().into_any_element());

        picker.focus_handle(cx).focus(window, cx);

        content.into_any_element()
    }

    fn render_create_remote_server(
        &self,
        state: &CreateRemoteServer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let ssh_prompt = state.ssh_prompt.clone();

        state.address_editor.update(cx, |editor, cx| {
            if editor.text(cx).is_empty() {
                editor.set_placeholder_text("ssh user@example -p 2222", window, cx);
            }
        });

        let theme = cx.theme();

        v_flex()
            .track_focus(&self.focus_handle(cx))
            .id("create-remote-server")
            .overflow_hidden()
            .size_full()
            .flex_1()
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(theme.colors().border_variant)
                    .child(state.address_editor.clone()),
            )
            .child(
                h_flex()
                    .bg(theme.colors().editor_background)
                    .rounded_b_sm()
                    .w_full()
                    .map(|this| {
                        if let Some(ssh_prompt) = ssh_prompt {
                            this.child(h_flex().w_full().child(ssh_prompt))
                        } else if let Some(address_error) = &state.address_error {
                            this.child(
                                h_flex().p_2().w_full().gap_2().child(
                                    Label::new(address_error.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Error),
                                ),
                            )
                        } else {
                            this.child(
                                h_flex()
                                    .p_2()
                                    .w_full()
                                    .gap_1()
                                    .child(
                                        Label::new(
                                            "Enter the command you use to SSH into this server.",
                                        )
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                    )
                                    .child(
                                        Button::new("learn-more", "Learn More")
                                            .label_size(LabelSize::Small)
                                            .end_icon(
                                                Icon::new(IconName::ArrowUpRight)
                                                    .size(IconSize::XSmall),
                                            )
                                            .on_click(|_, _, cx| {
                                                cx.open_url(
                                                    "https://zed.dev/docs/remote-development",
                                                );
                                            }),
                                    ),
                            )
                        }
                    }),
            )
    }

    #[cfg(target_os = "windows")]
    fn render_add_wsl_distro(
        &self,
        state: &AddWslDistro,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let connection_prompt = state.connection_prompt.clone();

        state.picker.update(cx, |picker, cx| {
            picker.focus_handle(cx).focus(window, cx);
        });

        v_flex()
            .id("add-wsl-distro")
            .overflow_hidden()
            .size_full()
            .flex_1()
            .map(|this| {
                if let Some(connection_prompt) = connection_prompt {
                    this.child(connection_prompt)
                } else {
                    this.child(state.picker.clone())
                }
            })
    }

    fn render_view_options(
        &mut self,
        options: ViewServerOptionsState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let last_entry = options.entries().last().unwrap();

        let mut view = Navigable::new(
            div()
                .track_focus(&self.focus_handle(cx))
                .size_full()
                .child(match &options {
                    ViewServerOptionsState::Ssh { connection, .. } => SshConnectionHeader {
                        connection_string: connection.host.to_string().into(),
                        paths: Default::default(),
                        nickname: connection.nickname.clone().map(|s| s.into()),
                        is_wsl: false,
                        is_devcontainer: false,
                    }
                    .render(window, cx)
                    .into_any_element(),
                    ViewServerOptionsState::Wsl { connection, .. } => SshConnectionHeader {
                        connection_string: connection.distro_name.clone().into(),
                        paths: Default::default(),
                        nickname: None,
                        is_wsl: true,
                        is_devcontainer: false,
                    }
                    .render(window, cx)
                    .into_any_element(),
                })
                .child(
                    v_flex()
                        .pb_1()
                        .child(ListSeparator)
                        .map(|this| match &options {
                            ViewServerOptionsState::Ssh {
                                connection,
                                entries,
                                server_index,
                            } => this.child(self.render_edit_ssh(
                                connection,
                                *server_index,
                                entries,
                                window,
                                cx,
                            )),
                            ViewServerOptionsState::Wsl {
                                connection,
                                entries,
                                server_index,
                            } => this.child(self.render_edit_wsl(
                                connection,
                                *server_index,
                                entries,
                                window,
                                cx,
                            )),
                        })
                        .child(ListSeparator)
                        .child({
                            div()
                                .id("ssh-options-copy-server-address")
                                .track_focus(&last_entry.focus_handle)
                                .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                                    this.mode = Mode::default_mode(&this.ssh_config_servers, cx);
                                    cx.focus_self(window);
                                    cx.notify();
                                }))
                                .child(
                                    ListItem::new("go-back")
                                        .toggle_state(
                                            last_entry.focus_handle.contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ui::ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::ArrowLeft).color(Color::Muted),
                                        )
                                        .child(Label::new("Go Back"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.mode =
                                                Mode::default_mode(&this.ssh_config_servers, cx);
                                            cx.focus_self(window);
                                            cx.notify()
                                        })),
                                )
                        }),
                )
                .into_any_element(),
        );

        for entry in options.entries() {
            view = view.entry(entry.clone());
        }

        view.render(window, cx).into_any_element()
    }

    fn render_edit_wsl(
        &self,
        connection: &WslConnectionOptions,
        index: WslServerIndex,
        entries: &[NavigableEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let distro_name = SharedString::new(connection.distro_name.clone());

        v_flex().child({
            fn remove_wsl_distro(
                remote_servers: Entity<RemoteServerProjects>,
                index: WslServerIndex,
                distro_name: SharedString,
                window: &mut Window,
                cx: &mut App,
            ) {
                let prompt_message = format!("Remove WSL distro `{}`?", distro_name);

                let confirmation = window.prompt(
                    PromptLevel::Warning,
                    &prompt_message,
                    None,
                    &["Yes, remove it", "No, keep it"],
                    cx,
                );

                cx.spawn(async move |cx| {
                    if confirmation.await.ok() == Some(0) {
                        remote_servers.update(cx, |this, cx| {
                            this.delete_wsl_distro(index, cx);
                        });
                        remote_servers.update(cx, |this, cx| {
                            this.mode = Mode::default_mode(&this.ssh_config_servers, cx);
                            cx.notify();
                        });
                    }
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
            }
            div()
                .id("wsl-options-remove-distro")
                .track_focus(&entries[0].focus_handle)
                .on_action(cx.listener({
                    let distro_name = distro_name.clone();
                    move |_, _: &menu::Confirm, window, cx| {
                        remove_wsl_distro(cx.entity(), index, distro_name.clone(), window, cx);
                        cx.focus_self(window);
                    }
                }))
                .child(
                    ListItem::new("remove-distro")
                        .toggle_state(entries[0].focus_handle.contains_focused(window, cx))
                        .inset(true)
                        .spacing(ui::ListItemSpacing::Sparse)
                        .start_slot(Icon::new(IconName::Trash).color(Color::Error))
                        .child(Label::new("Remove Distro").color(Color::Error))
                        .on_click(cx.listener(move |_, _, window, cx| {
                            remove_wsl_distro(cx.entity(), index, distro_name.clone(), window, cx);
                            cx.focus_self(window);
                        })),
                )
        })
    }

    fn render_edit_ssh(
        &self,
        connection: &SshConnectionOptions,
        index: SshServerIndex,
        entries: &[NavigableEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let connection_string = SharedString::new(connection.host.to_string());

        v_flex()
            .child({
                let label = if connection.nickname.is_some() {
                    "Edit Nickname"
                } else {
                    "Add Nickname to Server"
                };
                div()
                    .id("ssh-options-add-nickname")
                    .track_focus(&entries[0].focus_handle)
                    .on_action(cx.listener(move |this, _: &menu::Confirm, window, cx| {
                        this.mode = Mode::EditNickname(EditNicknameState::new(index, window, cx));
                        cx.notify();
                    }))
                    .child(
                        ListItem::new("add-nickname")
                            .toggle_state(entries[0].focus_handle.contains_focused(window, cx))
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(Icon::new(IconName::Pencil).color(Color::Muted))
                            .child(Label::new(label))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.mode =
                                    Mode::EditNickname(EditNicknameState::new(index, window, cx));
                                cx.notify();
                            })),
                    )
            })
            .child({
                let workspace = self.workspace.clone();
                fn callback(
                    workspace: WeakEntity<Workspace>,
                    connection_string: SharedString,
                    cx: &mut App,
                ) {
                    cx.write_to_clipboard(ClipboardItem::new_string(connection_string.to_string()));
                    workspace
                        .update(cx, |this, cx| {
                            struct SshServerAddressCopiedToClipboard;
                            let notification = format!(
                                "Copied server address ({}) to clipboard",
                                connection_string
                            );

                            this.show_toast(
                                Toast::new(
                                    NotificationId::composite::<SshServerAddressCopiedToClipboard>(
                                        connection_string.clone(),
                                    ),
                                    notification,
                                )
                                .autohide(),
                                cx,
                            );
                        })
                        .ok();
                }
                div()
                    .id("ssh-options-copy-server-address")
                    .track_focus(&entries[1].focus_handle)
                    .on_action({
                        let connection_string = connection_string.clone();
                        let workspace = self.workspace.clone();
                        move |_: &menu::Confirm, _, cx| {
                            callback(workspace.clone(), connection_string.clone(), cx);
                        }
                    })
                    .child(
                        ListItem::new("copy-server-address")
                            .toggle_state(entries[1].focus_handle.contains_focused(window, cx))
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(Icon::new(IconName::Copy).color(Color::Muted))
                            .child(Label::new("Copy Server Address"))
                            .end_slot(Label::new(connection_string.clone()).color(Color::Muted))
                            .show_end_slot_on_hover()
                            .on_click({
                                let connection_string = connection_string.clone();
                                move |_, _, cx| {
                                    callback(workspace.clone(), connection_string.clone(), cx);
                                }
                            }),
                    )
            })
            .child({
                fn remove_ssh_server(
                    remote_servers: Entity<RemoteServerProjects>,
                    index: SshServerIndex,
                    connection_string: SharedString,
                    window: &mut Window,
                    cx: &mut App,
                ) {
                    let prompt_message = format!("Remove server `{}`?", connection_string);

                    let confirmation = window.prompt(
                        PromptLevel::Warning,
                        &prompt_message,
                        None,
                        &["Yes, remove it", "No, keep it"],
                        cx,
                    );

                    cx.spawn(async move |cx| {
                        if confirmation.await.ok() == Some(0) {
                            remote_servers.update(cx, |this, cx| {
                                this.delete_ssh_server(index, cx);
                            });
                            remote_servers.update(cx, |this, cx| {
                                this.mode = Mode::default_mode(&this.ssh_config_servers, cx);
                                cx.notify();
                            });
                        }
                        anyhow::Ok(())
                    })
                    .detach_and_log_err(cx);
                }
                div()
                    .id("ssh-options-copy-server-address")
                    .track_focus(&entries[2].focus_handle)
                    .on_action(cx.listener({
                        let connection_string = connection_string.clone();
                        move |_, _: &menu::Confirm, window, cx| {
                            remove_ssh_server(
                                cx.entity(),
                                index,
                                connection_string.clone(),
                                window,
                                cx,
                            );
                            cx.focus_self(window);
                        }
                    }))
                    .child(
                        ListItem::new("remove-server")
                            .toggle_state(entries[2].focus_handle.contains_focused(window, cx))
                            .inset(true)
                            .spacing(ui::ListItemSpacing::Sparse)
                            .start_slot(Icon::new(IconName::Trash).color(Color::Error))
                            .child(Label::new("Remove Server").color(Color::Error))
                            .on_click(cx.listener(move |_, _, window, cx| {
                                remove_ssh_server(
                                    cx.entity(),
                                    index,
                                    connection_string.clone(),
                                    window,
                                    cx,
                                );
                                cx.focus_self(window);
                            })),
                    )
            })
    }

    fn render_edit_nickname(
        &self,
        state: &EditNicknameState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(connection) = RemoteSettings::get_global(cx)
            .ssh_connections()
            .nth(state.index.0)
        else {
            return v_flex()
                .id("ssh-edit-nickname")
                .track_focus(&self.focus_handle(cx));
        };

        let connection_string = connection.host.clone();
        let nickname = connection.nickname.map(|s| s.into());

        v_flex()
            .id("ssh-edit-nickname")
            .track_focus(&self.focus_handle(cx))
            .child(
                SshConnectionHeader {
                    connection_string: connection_string.into(),
                    paths: Default::default(),
                    nickname,
                    is_wsl: false,
                    is_devcontainer: false,
                }
                .render(window, cx),
            )
            .child(
                h_flex()
                    .p_2()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(state.editor.clone()),
            )
    }

    fn render_default(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .min_h(rems(20.))
            .size_full()
            .child(self.default_picker.clone())
            .into_any_element()
    }

    fn create_host_from_ssh_config(
        &mut self,
        ssh_config_host: &SharedString,
        cx: &mut Context<'_, Self>,
    ) -> SshServerIndex {
        let new_ix = RemoteSettings::get_global(cx).ssh_connections().count();

        self.add_ssh_server(
            SshConnectionOptions {
                host: ssh_config_host.to_string().into(),
                ..SshConnectionOptions::default()
            },
            cx,
        );
        self.mode = Mode::default_mode(&self.ssh_config_servers, cx);
        SshServerIndex(new_ix)
    }
}

fn spawn_ssh_config_watch(fs: Arc<dyn Fs>, cx: &Context<RemoteServerProjects>) -> Task<()> {
    enum ConfigSource {
        User(String),
        Global(String),
    }

    let mut streams = Vec::new();
    let mut tasks = Vec::new();

    // Setup User Watcher
    let user_path = user_ssh_config_file();
    info!("SSH: Watching User Config at: {:?}", user_path);

    // We clone 'fs' here because we might need it again for the global watcher.
    let (user_s, user_t) = watch_config_file(cx.background_executor(), fs.clone(), user_path);
    streams.push(user_s.map(ConfigSource::User).boxed());
    tasks.push(user_t);

    // Setup Global Watcher
    if let Some(gp) = global_ssh_config_file() {
        info!("SSH: Watching Global Config at: {:?}", gp);
        let (global_s, global_t) =
            watch_config_file(cx.background_executor(), fs, gp.to_path_buf());
        streams.push(global_s.map(ConfigSource::Global).boxed());
        tasks.push(global_t);
    } else {
        debug!("SSH: No Global Config defined.");
    }

    // Combine into a single stream so that only one is parsed at once.
    let mut merged_stream = futures::stream::select_all(streams);

    cx.spawn(async move |remote_server_projects, cx| {
        let _tasks = tasks; // Keeps the background watchers alive
        let mut global_hosts = BTreeSet::default();
        let mut user_hosts = BTreeSet::default();

        while let Some(event) = merged_stream.next().await {
            match event {
                ConfigSource::Global(content) => {
                    global_hosts = parse_ssh_config_hosts(&content);
                }
                ConfigSource::User(content) => {
                    user_hosts = parse_ssh_config_hosts(&content);
                }
            }

            // Sync to Model
            if remote_server_projects
                .update(cx, |project, cx| {
                    project.ssh_config_servers = global_hosts
                        .iter()
                        .chain(user_hosts.iter())
                        .map(SharedString::from)
                        .collect();
                    let ssh_config_servers = project.ssh_config_servers.clone();
                    let (has_open_project, is_local) =
                        RemoteServerProjects::workspace_flags(&project.workspace, cx);
                    project.default_picker.update(cx, |picker, cx| {
                        picker
                            .delegate
                            .reload(&ssh_config_servers, has_open_project, is_local, cx);
                        cx.notify();
                    });
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
        }
    })
}

fn get_text(element: &Entity<Editor>, cx: &mut App) -> String {
    element.read(cx).text(cx).trim().to_string()
}

impl ModalView for RemoteServerProjects {
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> DismissDecision {
        DismissDecision::Dismiss(self.allow_dismissal)
    }
}

impl Focusable for RemoteServerProjects {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.mode {
            Mode::Default => self.default_picker.focus_handle(cx),
            Mode::ProjectPicker(picker) => picker.focus_handle(cx),
            _ => self.focus_handle.clone(),
        }
    }
}

impl EventEmitter<DismissEvent> for RemoteServerProjects {}

impl Render for RemoteServerProjects {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .elevation_3(cx)
            .w(rems(34.))
            .key_context("RemoteServerModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.focus_handle(cx).focus(window, cx);
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if matches!(this.mode, Mode::Default) {
                    cx.emit(DismissEvent)
                }
            }))
            .child(match &self.mode {
                Mode::Default => self.render_default(window, cx).into_any_element(),
                Mode::ViewServerOptions(state) => self
                    .render_view_options(state.clone(), window, cx)
                    .into_any_element(),
                Mode::ProjectPicker(element) => element.clone().into_any_element(),
                Mode::CreateRemoteServer(state) => self
                    .render_create_remote_server(state, window, cx)
                    .into_any_element(),
                Mode::CreateRemoteDevContainer(state) => self
                    .render_create_dev_container(state, window, cx)
                    .into_any_element(),
                Mode::EditNickname(state) => self
                    .render_edit_nickname(state, window, cx)
                    .into_any_element(),
                #[cfg(target_os = "windows")]
                Mode::AddWslDistro(state) => self
                    .render_add_wsl_distro(state, window, cx)
                    .into_any_element(),
            })
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    fn ssh_config_entry(host: &'static str) -> RemoteEntry {
        RemoteEntry::SshConfig {
            host: SharedString::from(host),
        }
    }

    #[test]
    fn test_filter_sync_repopulates_after_rebuild() {
        let entries = vec![ssh_config_entry("alpha"), ssh_config_entry("beta")];
        let mut state = DefaultState {
            filter_data: Arc::new(FilterData::build(&entries)),
            servers: entries,
            filtered_servers: None,
        };

        state.filter_sync("alp");
        let filtered = state.filtered_servers.as_ref().expect("should filter");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].server_index, 0);
        assert!(!filtered[0].host_positions.is_empty());

        // The filtered index resolves back into the original server list.
        match &state.servers[filtered[0].server_index] {
            RemoteEntry::SshConfig { host, .. } => assert_eq!(host.as_ref(), "alpha"),
            _ => panic!("expected SshConfig"),
        }

        state.filter_sync("");
        assert!(state.filtered_servers.is_none());
    }

    #[test]
    fn parses_quick_port_forward() {
        let forward = parse_quick_port_forward("127.0.0.1:3000").expect("valid port");
        assert_eq!(forward.local_host.as_deref(), Some("localhost"));
        assert_eq!(forward.local_port, 3000);
        assert_eq!(forward.remote_host.as_deref(), Some("127.0.0.1"));
        assert_eq!(forward.remote_port, 3000);
        assert!(parse_quick_port_forward("70000").is_err());
    }

    #[test]
    fn builds_ssh_control_commands() {
        let mut command = remote::CommandTemplate {
            program: "ssh".to_owned(),
            args: vec![
                "-N".to_owned(),
                "-L".to_owned(),
                "3000:localhost:3000".to_owned(),
                "server".to_owned(),
            ],
            env: Default::default(),
        };

        set_ssh_control_operation(&mut command, "forward").expect("valid SSH command");
        assert_eq!(
            command.args,
            ["-O", "forward", "-L", "3000:localhost:3000", "server"]
        );
    }

    #[test]
    fn formats_ssh_forward_specifications() {
        let forward = remote::SshPortForwardOption {
            local_host: Some("::1".to_owned()),
            local_port: 3000,
            remote_host: Some("2001:db8::1".to_owned()),
            remote_port: 4000,
        };

        assert_eq!(
            format_port_forward_specification(&forward),
            "[::1]:3000:[2001:db8::1]:4000"
        );
    }

    #[test]
    fn parses_remote_listening_processes() {
        let output = concat!(
            "LISTEN 0 511 127.0.0.1:5001 0.0.0.0:* users:((\"node\",pid=123,fd=20))\n",
            "LISTEN 0 4096 0.0.0.0:3002 0.0.0.0:*\n",
            "python 456 user 3u IPv4 0t0 TCP *:8000 (LISTEN)\n",
        );
        let processes = parse_listening_processes(output);
        assert_eq!(
            processes.get(&5001).map(AsRef::as_ref),
            Some("node (PID 123)")
        );
        assert_eq!(
            processes.get(&8000).map(AsRef::as_ref),
            Some("python (PID 456)")
        );
        assert_eq!(processes.get(&3002).map(AsRef::as_ref), Some(""));
        assert!(!processes.contains_key(&1111));
    }
}

#[cfg(test)]
mod create_host_tests {
    use super::*;
    use extension::ExtensionHostProxy;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use http_client::BlockedHttpClient;
    use node_runtime::NodeRuntime;
    use remote_server::{HeadlessAppState, HeadlessProject};
    use serde_json::json;
    use util::path;

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            let state = AppState::test(cx);
            crate::init(cx);
            editor::init(cx);
            state
        })
    }

    #[gpui::test]
    async fn ports_panel_is_disabled_for_local_workspaces(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let project = workspace.project().downgrade();
            cx.new(|cx| PortsPanel::new(project, window, cx))
        });

        assert!(!panel.read_with(cx, |panel, cx| panel.enabled(cx)));
    }

    /// The weak project handle must fail closed: a dropped project means
    /// disabled, not a panic.
    #[gpui::test]
    async fn ports_panel_is_disabled_once_the_project_is_dropped(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));

        // A project the workspace does not hold, so dropping it really drops it.
        let orphan = Project::test(app_state.fs.clone(), [], cx).await;
        let orphan_weak = orphan.downgrade();
        let panel = workspace.update_in(cx, |_, window, cx| {
            cx.new(|cx| PortsPanel::new(orphan_weak, window, cx))
        });
        drop(orphan);
        cx.run_until_parked();

        assert!(panel.read_with(cx, |panel, _| panel.project.upgrade().is_none()));
        assert!(!panel.read_with(cx, |panel, cx| panel.enabled(cx)));
        assert!(panel.read_with(cx, |panel, cx| panel.active_server(cx).is_none()));
        // The other weak-handle users must not panic either.
        panel.update_in(cx, |panel, window, cx| {
            assert!(panel.icon_label(window, cx).is_none());
            panel.refresh(cx);
            panel.add_port(window, cx);
            panel.remove_port(0, cx);
        });
    }

    #[gpui::test]
    async fn toggling_the_bottom_dock_does_not_lease_the_workspace_twice(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let project = workspace.project().downgrade();
            cx.new(|cx| PortsPanel::new(project, window, cx))
        });

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel, window, cx);
            workspace.toggle_dock(DockPosition::Bottom, window, cx);
        });
        cx.run_until_parked();

        assert!(workspace.read_with(cx, |workspace, cx| {
            workspace.bottom_dock().read(cx).is_open()
        }));
    }

    /// Every path that reaches `Panel::enabled` while the Workspace entity is
    /// leased: all three dock positions, open and close, and the ToggleFocus
    /// action. Any of these double-leasing aborts the process.
    #[gpui::test]
    async fn panel_trait_methods_never_read_the_leased_workspace(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let project = workspace.project().downgrade();
            cx.new(|cx| PortsPanel::new(project, window, cx))
        });

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
        });
        cx.run_until_parked();

        for position in [
            DockPosition::Left,
            DockPosition::Bottom,
            DockPosition::Right,
        ] {
            for _ in 0..2 {
                workspace.update_in(cx, |workspace, window, cx| {
                    workspace.toggle_dock(position, window, cx);
                });
                cx.run_until_parked();
            }
            // Back where it started after open + close.
            assert!(!workspace.read_with(cx, |workspace, cx| {
                workspace.dock_at_position(position).read(cx).is_open()
            }));
        }

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<PortsPanel>(window, cx);
        });
        cx.run_until_parked();

        // `icon_label` also calls `active_server`; the status bar reads it.
        panel.update_in(cx, |panel, window, cx| {
            assert!(panel.icon_label(window, cx).is_none());
        });
    }

    #[gpui::test]
    async fn new_ssh_server_opens_folder_picker(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));
        server_cx.update(|cx| release_channel::init(semver::Version::new(0, 0, 0), cx));

        let (mock_options, server_session, connect_guard) =
            RemoteClient::fake_server(cx, server_cx);
        let server_executor = server_cx.executor();
        let remote_fs = FakeFs::new(server_executor.clone());
        remote_fs
            .insert_tree(
                path!("/home/user"),
                json!({
                    "project": {},
                }),
            )
            .await;

        server_cx.update(HeadlessProject::init);
        let _headless_project = server_cx.new(|cx| {
            HeadlessProject::new(
                HeadlessAppState {
                    session: server_session,
                    fs: remote_fs,
                    http_client: Arc::new(BlockedHttpClient),
                    node_runtime: NodeRuntime::unavailable(),
                    languages: Arc::new(language::LanguageRegistry::new(server_executor)),
                    extension_host_proxy: Arc::new(ExtensionHostProxy::new()),
                    startup_time: std::time::Instant::now(),
                },
                false,
                cx,
            )
        });
        drop(connect_guard);
        let client = RemoteClient::connect_mock(mock_options, cx).await;

        let fs: Arc<dyn Fs> = app_state.fs.clone();
        let project = Project::test(fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let modal = workspace.update_in(cx, |workspace, window, cx| {
            let weak = cx.entity().downgrade();
            workspace.toggle_modal(window, cx, |window, cx| {
                RemoteServerProjects::new(false, fs, window, weak, cx)
            });
            workspace
                .active_modal::<RemoteServerProjects>(cx)
                .expect("remote server modal should be active")
        });

        modal.update_in(cx, |modal, window, cx| {
            modal.finish_create_ssh_server(
                SshConnectionOptions {
                    host: "new.example".into(),
                    ..Default::default()
                },
                client,
                window,
                cx,
            );
        });
        drop(modal);
        cx.run_until_parked();

        let active_modal = workspace
            .update(cx, |workspace, cx| {
                workspace.active_modal::<RemoteServerProjects>(cx)
            })
            .expect("remote server modal should remain active");
        let connection_string = active_modal.update(cx, |modal, cx| {
            let picker = match &modal.mode {
                Mode::ProjectPicker(picker) => picker,
                _ => panic!("new SSH server should proceed to folder selection"),
            };
            match &picker.read(cx).data {
                ProjectPickerData::Ssh {
                    connection_string, ..
                } => connection_string.clone(),
                ProjectPickerData::Wsl { .. } => panic!("expected SSH folder picker"),
            }
        });
        assert_eq!(connection_string.as_ref(), "new.example");

        let connections = cx.update(|_, cx| {
            RemoteSettings::get_global(cx)
                .ssh_connections()
                .collect::<Vec<_>>()
        });
        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].host, "new.example");
    }

    #[gpui::test]
    async fn test_create_host_from_ssh_config_returns_new_connection_index(
        cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        let fs: Arc<dyn Fs> = app_state.fs.clone();

        cx.update(|cx| {
            update_settings_file(fs.clone(), cx, |settings, _| {
                settings.remote.ssh_connections = Some(vec![SshConnection {
                    host: "host-a.example".to_string(),
                    projects: BTreeSet::from_iter([RemoteProject {
                        paths: vec!["/path/to/project-a".to_string()],
                    }]),
                    ..Default::default()
                }]);
            });
        });
        cx.run_until_parked();

        let project = Project::test(fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));

        let modal = workspace.update_in(cx, |_workspace, window, cx| {
            let weak = cx.weak_entity();
            cx.new(|cx| RemoteServerProjects::new(false, fs.clone(), window, weak, cx))
        });

        let host_b = SharedString::from("host-b.example");
        let new_index = modal.update(cx, |modal, cx| {
            modal.create_host_from_ssh_config(&host_b, cx)
        });
        cx.run_until_parked();

        let connections = cx.update(|_, cx| {
            RemoteSettings::get_global(cx)
                .ssh_connections()
                .collect::<Vec<_>>()
        });

        assert_eq!(connections.len(), 2);
        assert_eq!(connections[0].host, "host-a.example");
        assert_eq!(connections[1].host, "host-b.example");
        assert_eq!(
            connections[new_index.0].host, "host-b.example",
            "returned index should point at the newly created host"
        );

        assert_eq!(connections[0].projects.len(), 1);
        assert!(connections[new_index.0].projects.is_empty());
    }
}
