# Product

<!-- impeccable:product-schema 1 -->

This document gives contributors a durable, public description of ZOrca and the
claims that may be made on the website. The `README.md` remains the primary
source for installation instructions and current project status.

## Platform

web

The product documented by the website is a native desktop application for macOS
and Linux. Windows is not supported.

## Users

ZOrca is intended for developers who:

- run multiple coding-agent sessions across real repositories;
- want worktree isolation between concurrent agents;
- work with local and SSH-connected projects;
- want project and agent-session context restored after restarting or reconnecting;
- want terminals, files, diffs, staging, commits, and history together; and
- prefer direct control over agent work.

Contributors are a second audience. Public documentation should make the
product's current scope, limitations, and relationship to upstream Zed clear.

## Product Purpose

ZOrca is a native Agent Development Environment that brings projects, Git
worktrees, coding-agent terminals, files, and review tools into one workspace.
It is built on Zed using Rust and GPUI.

Each agent can work in its own Git worktree and terminal context. Developers can
move between local or SSH-connected projects, inspect associated changes, stage
files, commit, and browse history without leaving the workspace. ZOrca persists
the project tree, active workspace, and agent terminal sessions so that context
can be restored after the app or connection returns.

ZOrca uses agent command-line tools that developers already install and
authenticate. It does not require a ZOrca account or hosted ZOrca AI service.

## Positioning

ZOrca combines Zed's native editor foundation with an agent-first,
worktree-per-agent workspace. It is a hands-on environment for developers
supervising their own agents, rather than a fleet orchestration or hosted AI
service.

ZOrca is an independent hard fork of
[Zed](https://github.com/zed-industries/zed). It follows a separate product
direction and release lifecycle and does not promise routine synchronization
with upstream; applicable changes may be ported selectively.

ZOrca is inspired by the workspace-first approach of
[Orca](https://github.com/stablyai/orca), but it is a separate project. ZOrca is
not affiliated with or endorsed by Zed Industries or Stably AI.

## Operating Context

Developers work across local and SSH-connected repositories, open isolated Git
worktrees for coding agents, run agent command-line tools in worktree-scoped
terminals, and review, stage, commit, and browse changes in the same native
workspace. Project groups, active workspace state, and persistent terminal
sessions are restored when developers reopen or reconnect.

The current installation path is building from source, as documented in
`README.md`. `script/bundle-mac` produces a local `ZOrca.app` bundle.

The public website is a static HTML and CSS site in `website/` with no build
step. It remains self-contained within the Rust repository, and its installation
copy must match the repository's published release state.

## Capabilities and Constraints

Current capabilities:

- Multiple projects and worktrees in one sidebar
- Local and remote workspaces over SSH
- Persistent project groups, active workspace state, and agent terminal sessions
- Agent terminals in the centre pane, scoped to project worktrees
- Multiple terminal tabs per project worktree
- Diff inspection, staging, commits, and history for the active worktree
- Zed's editor foundation, including LSP, tree-sitter, multibuffers, debugging,
  Vim keybindings, themes, and extensions
- Optional GitHub Copilot support
- No required account or hosted ZOrca AI service

ZOrca is pre-alpha. Breaking changes are expected. There are no packaged,
signed releases or automatic updates yet.

Before release, do not present packaged releases, signing, notarization,
Homebrew distribution, automatic updates, or additional Linux formats as
available.
Public copy can identify Homebrew distribution as planned without a date.

The following areas are planned, not current capabilities:

- ZOrca will start a server that manages sessions, terminals, agents, and
  workspaces through a REST API. The CLI will use the same API. The project
  will evaluate OpenAPI for client generation.
- A mobile app will manage sessions, terminals, agents, and workspaces on a Mac
  or host that the user controls. It will connect over Tailscale or a local
  network.
- Cron schedules will start agents with automation prompts. The agents can run
  on the local machine or an SSH-connected remote host.

The project will evaluate a Cloudflare stack for mobile access, remote
schedules, and API coordination. The final deployment model is not set.

Do not present planned work as released. If the project has no approved public
release plan, do not attach dates.

ZOrca does not currently provide fleet orchestration, scheduled automation, a
mobile app, or a public REST API for session management.

## Brand Commitments

The product name is **ZOrca** and the public website is
[zorca.net](https://zorca.net). The logo combines a white fused `ZO` symbol with
a cobalt, violet, and coral field. Asset usage, palette, typography, layout, and
accessibility requirements are documented in `DESIGN.md`.

ZOrca is licensed under GPL-3.0-or-later. Preserve upstream copyright and
licence notices as described in the repository's licence files.

## Evidence on Hand

- The current product implementation and public claims in the repository
- Installation and current-status documentation in `README.md`
- Authentic application captures in `website/`
- Brand and interface assets documented in `DESIGN.md`
- The Zed foundation inherited by the hard fork

Do not fabricate users, testimonials, metrics, dates, prices, benchmarks, or
release claims. Product captures must not contain private information.

## Product Principles

1. Lead with the product and its worktree-per-agent workflow.
2. Distinguish current capabilities from planned direction.
3. State pre-alpha and distribution limitations plainly.
4. Use authentic application captures without private information.
5. Credit upstream work and describe competing projects factually.
6. Keep claims consistent with the application, `README.md`, and release state.

## Accessibility & Inclusion

ZOrca inherits Zed's accessibility foundation. The public website must preserve
the accessibility requirements documented in `DESIGN.md`, including semantic
structure, keyboard access, visible focus, descriptive alternatives, WCAG AA
text contrast, reduced-motion support, and minimum touch-target sizing.
