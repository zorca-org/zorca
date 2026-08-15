# Contributing to ZOrca

ZOrca is pre-alpha and moves quickly. Issues and pull requests are welcome.

ZOrca is an independent hard fork of
[Zed](https://github.com/zed-industries/zed) and is not affiliated with Zed
Industries. Contributions here are **not** covered by Zed's contributor licence
agreement, and they do not reach upstream Zed. ZOrca does not routinely
synchronize with upstream; applicable fixes may be ported selectively. Changes
that also apply cleanly to Zed may be proposed to
[zed-industries/zed](https://github.com/zed-industries/zed) separately.

## Before you start

Open an issue before writing a large change. The fork carries a wide diff
against upstream, and a patch that conflicts with the direction of that diff is
painful to merge no matter how good it is.

Small, self-contained changes need no discussion first:

- Bug fixes
- Documentation corrections
- Keybindings and actions

## Building

See [Build from source](./README.md#build-from-source). The Cargo package is
`zed`, inherited from upstream; the binary it produces is `zorca`. The
repository-local `cargo zorca` alias runs that package.

The generated [architecture report](./graphify-out/GRAPH_REPORT.md) and
[interactive graph](./graphify-out/graph.html) provide a high-level map of the
workspace and crate dependencies. Run `graphify query "<question>"` for a
focused view of `graphify-out/graph.json`.

## Before you open a pull request

Enable the repository's commit checks once per clone:

```sh
git config --local core.hooksPath .githooks
```

The pre-commit hook checks formatting and runs the same Clippy wrapper as CI.
Use `--no-verify` only when you intentionally need to defer those checks.

```sh
./script/clippy          # not `cargo clippy` — the wrapper sets the lint config
cargo test -p <crate>    # the crates you touched
cargo zorca              # the app should actually start
```

That last one matters more than it looks. A keymap entry or settings default
naming something the code no longer defines will compile cleanly and then panic
on startup, so `cargo check` passing is not evidence that the app runs.

## Pull requests

Write the title as an imperative sentence, capitalised, with no trailing
punctuation and no conventional-commit prefix:

- `Fix crash in project panel`
- `git_ui: Add history view` — prefix with a crate name when one crate is the
  clear scope

End the body with a release-notes section, in exactly this shape:

```
Release Notes:

- Added ...
```

Use `- Added ...`, `- Fixed ...`, or `- Improved ...` for a user-facing change,
and `- N/A` for anything else.

## Code style

`.rules` holds the conventions this codebase enforces, including the Rust
guidelines and the GPUI primitives. It is worth reading before your first
change. `AGENTS.md`, `CLAUDE.md`, and `GEMINI.md` are symlinks to it, so coding
agents pick up the same rules.

The short version:

- Prefer correctness and clarity over speed unless asked otherwise
- Avoid `unwrap()`; propagate with `?`
- Never discard a fallible result with `let _ =`
- Comment *why*, not *what*
- Full words for variable names

## Licence

ZOrca is GPL-3.0-or-later. By contributing you agree that your contribution is
licensed under the same terms.
