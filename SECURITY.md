# Security policy

## Supported versions

ZOrca is pre-alpha and ships no releases. Only the current `main` branch is
supported. Fixes land there, and there are no backports.

## Reporting a vulnerability

Report privately, not in a public issue: open a [security
advisory](https://github.com/zorca-org/zorca/security/advisories/new) on this
repository.

Include what you did, what happened, and the commit you were on
(`git rev-parse --short HEAD`).

ZOrca is a fork of [Zed](https://github.com/zed-industries/zed). If the problem
also reproduces in upstream Zed, please report it to
[Zed's security process](https://github.com/zed-industries/zed/security) as
well — a fix there reaches far more people than a fix here.

## Scope

Worth reporting: anything that lets a repository, extension, or language server
run code you did not intend, read files outside the workspace, or exfiltrate
credentials.

Out of scope by design: the agent presets. `codex --yolo` and
`claude --dangerously-skip-permissions` intentionally disable the agent's own
permission prompts, and a Git worktree is not a sandbox. An agent doing
something destructive under those flags is the documented behaviour of the
flags, not a vulnerability in ZOrca.
