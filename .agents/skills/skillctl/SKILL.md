---
name: skillctl
description: Inspect managed skills, diagnose missing or stale projections, and guide users through skillctl lifecycle commands.
---

Use this skill when you need to inspect installed skills, explain why a skill is missing,
stale, shadowed, or detached, or guide the user through install, pin, rollback, sync,
cleanup, and doctor flows.

Preferred commands:

- `skillctl list`
- `skillctl path <skill>`
- `skillctl history [skill]`
- `skillctl update [skill]`
- `skillctl doctor`
- `skillctl explain <skill>`
- `skillctl sync`
- `skillctl clean`

When a user-managed skill named `skillctl` exists in user scope, treat that copy as the
active one instead of assuming the bundled asset is visible in every runtime root.
