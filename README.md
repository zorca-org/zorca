<p align="center">
  <img src="docs/branding/logos/zorca-logo-transparent.png" alt="ZOrca" width="104" height="104" />
</p>

<h1 align="center">ZOrca</h1>

<p align="center">
  <strong>All your agents. All your projects. One native workspace.</strong>
</p>

<p align="center">
  <a href="https://zorca.net"><strong>Website</strong></a> &middot;
  <a href="#install"><strong>Install</strong></a> &middot;
  <a href="#features"><strong>Features</strong></a> &middot;
  <a href="./CONTRIBUTING.md"><strong>Contributing</strong></a>
</p>

<p align="center">
  <a href="./LICENSE-GPL"><img src="https://img.shields.io/badge/license-GPL--3.0--or--later-304CFF" alt="GPL-3.0-or-later" /></a>
  <a href="#project-status"><img src="https://img.shields.io/badge/status-pre--alpha-FF654B" alt="Pre-alpha" /></a>
  <a href="#install"><img src="https://img.shields.io/badge/platforms-macOS%20%C2%B7%20Linux-713CFF" alt="macOS and Linux" /></a>
  <a href="https://github.com/zed-industries/zed"><img src="https://img.shields.io/badge/built%20on-Zed-071833" alt="Built on Zed" /></a>
</p>

<br />

<p align="center">
  <img src="website/zorca-workspace.webp" alt="ZOrca showing project worktrees, an agent terminal, and repository files" width="960" />
</p>

ZOrca is a native **Agent Development Environment** built on
[Zed](https://github.com/zed-industries/zed). Give every coding agent its own
Git worktree, terminal, and diff across local and SSH-connected projects, then
restore its project and session context when you return.

Use the agent CLI you already have. Keep its work visible. Review and commit
without rebuilding your development workflow around a terminal wrapper.

<p align="center">
  <strong>Zed + Orca = ZOrca</strong><br />
  <sub>Zed's native editor foundation with an Orca-inspired workspace-first approach.</sub>
</p>

## ZOrca is right for you if

- You run **Claude Code, Codex, or another terminal agent** across real repositories.
- You want several agents working in parallel **without sharing one checkout**.
- You work across **multiple local or SSH-connected projects**.
- You want projects and agent sessions to **resume after an app or connection restart**.
- You want the terminal, files, diff, staging, and history **in one native workspace**.
- You want Zed's editor depth without making hosted AI the centre of the product.

## How it works

| | Step | What stays together |
| --- | --- | --- |
| **01** | Open a project | Repository, local or remote connection, and worktrees |
| **02** | Start an agent | One worktree with as many terminal tabs as you need |
| **03** | Review the result | Active terminal, files, diff, staging, commit, and history |
| **04** | Return later | Project tree, active workspace, and persistent terminal sessions |

## Features

### Resume projects and sessions

Keep local and SSH-connected repositories in one sidebar. ZOrca saves the
project tree, active workspace, and agent terminal sessions, then restores that
context when the app or connection returns. Persistent sessions reconnect when
available or can be recreated in place.

<p align="center">
  <a href="website/zorca-projects.mp4"><img src="docs/images/zorca-projects-demo.gif" alt="ZOrca switching between coding agents in different projects" width="720" /></a>
</p>

### Give every agent its own worktree

Agents work in separate Git worktrees instead of colliding in one checkout.
Each workspace keeps its terminal context and working files attributable to the
agent using it.

<p align="center">
  <a href="website/zorca-worktree.mp4"><img src="docs/images/zorca-worktree-demo.gif" alt="ZOrca opening an isolated project worktree and its terminal context" width="720" /></a>
</p>

### Review terminals and changes together

The Git cockpit follows the active workspace. Inspect diffs, stage changes,
commit, and browse history without losing the agent terminal that produced the
work.

<p align="center">
  <a href="website/zorca-git-review.mp4"><img src="docs/images/zorca-git-review-demo.gif" alt="ZOrca reviewing tracked and untracked changes beside an agent terminal" width="720" /></a>
</p>

## Zed's editor foundation, already built in

ZOrca reorganizes Zed around agent workspaces instead of rebuilding the editor
underneath them.

| Editing | Navigation and tooling |
| --- | --- |
| LSP and tree-sitter | Debugger and multibuffers |
| Native Vim keybindings | Beautiful themes |
| Zed Extensions | `zed://` URLs |

Also included today:

- Multiple repositories and worktrees in one sidebar
- Local and remote workspaces over SSH
- Persistent projects, workspace state, and agent terminal sessions
- As many agent terminal tabs as needed per project worktree
- Git diff, staging, commit, and history workflows
- No hosted AI or required ZOrca account; GitHub Copilot remains optional

## Install

> [!NOTE]
> ZOrca is pre-alpha. There are no packaged or signed releases yet, so the
> current installation path builds from source on macOS or Linux. Homebrew is
> planned.

### macOS

Install the prerequisites from [Zed's macOS development guide](https://zed.dev/docs/development/macos), then:

```sh
git clone https://github.com/zorca-org/zorca.git
cd zorca
cargo zorca
```

To build a `ZOrca.app` bundle:

```sh
script/bundle-mac
```

### Linux

Follow [Zed's Linux development guide](https://zed.dev/docs/development/linux), then:

```sh
git clone https://github.com/zorca-org/zorca.git
cd zorca
./script/linux
./script/download-wasi-sdk
cargo zorca
```

## Where ZOrca fits

ZOrca combines Zed's mature editor with agent-first workspaces. Zed is the
polished upstream editor; [Orca](https://github.com/stablyai/orca) is the broader
ADE today.

| Capability | ZOrca | Zed | Orca |
| --- | --- | --- | --- |
| **Workspace model** | Agent workspaces with Git worktrees | Project folders | Agent workspaces |
| **Remote projects** | SSH workspaces | SSH projects | Remote workspaces |
| **Agent terminals** | As many as needed per project worktree | Terminal dock | First-class |
| **Automation and orchestration** | No; hands-on control only | No | Yes |
| **Built-in AI** | None hosted; Copilot optional | Zed AI, hosted | Bring your own |
| **Availability** | Desktop from source; mobile planned | Desktop app | Desktop and mobile apps |

Choose **Zed** for the polished upstream editor and Zed's hosted AI. Choose
**Orca** for fleet orchestration, automation, and mobile control. Choose
**ZOrca** for Zed's native editor with a worktree-first layout and close,
hands-on control of agents across projects.

## Coming soon

These are directions, not first-version capabilities. There are no promised dates.

- **Agent communication bus** — workspace-scoped messaging so agents can exchange context and coordinate work
- **Deeper agent integration** — better monitoring and direct interaction with running coding agents
- **Mobile companion** — monitor and interact with workspaces away from the desktop

## Relationship to Zed

ZOrca is an independent hard fork of Zed. It inherits Zed's Rust and GPUI
foundation, editing engine, terminal rendering, Git tooling, extensions,
accessibility, persistence, and platform support, but follows a separate product
direction and release lifecycle.

The fork reorganizes the workspace around projects, worktrees, and centre-pane
agent terminals. Zed's hosted AI is removed; GitHub Copilot, the extension API,
and `zed://` URLs remain available.

ZOrca does not promise compatibility with or routine synchronization from
upstream Zed. Upstream fixes and improvements may be ported selectively when
they fit ZOrca's direction. ZOrca-specific issues and contributions belong in
this repository; changes that also apply cleanly to Zed may be proposed to
[zed-industries/zed](https://github.com/zed-industries/zed) separately.

## Project status

ZOrca is pre-alpha. Expect breaking changes and expect to build it yourself.

- No signed releases or automatic updates
- Windows is not supported
- Fleet orchestration and scheduled automation are not current capabilities

ZOrca is an independent hard fork. It is not affiliated with or endorsed by Zed
Industries or Stably AI.

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) to build, test, and contribute.

## License

ZOrca is licensed under GPL-3.0-or-later. Upstream copyright and license notices
are preserved; see [LICENSE-GPL](./LICENSE-GPL) and
[LICENSE-APACHE](./LICENSE-APACHE).
