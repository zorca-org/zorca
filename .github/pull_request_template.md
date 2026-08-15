# Objective

- What does this change do, and why?
- Link the issue it closes with "Fixes #X".

## Solution

- How does it work?

## Testing

- How did you test this, and on which platform?
- What should a reviewer do to verify it?

## Checklist

- [ ] `./script/clippy` passes
- [ ] Tests cover the new or changed behaviour
- [ ] The app starts (`cargo run -p zed`) — a keymap entry or settings default
      naming something the code no longer defines compiles fine, then panics at
      startup
- [ ] Unsafe blocks, if any, have justifying comments

## Showcase

> Optional. Delete this section if the change has no visual result.

Screenshots or a recording, ideally before/after for changes to existing
features.

---

Release Notes:

- N/A or Added/Fixed/Improved ...
