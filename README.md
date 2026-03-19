# skillctl

`skillctl` is a local-first, cross-agent skill manager for the open `SKILL.md` ecosystem.

It gives one operating model for canonical workspace skills, imported skills, overlays, projections, local history, diagnostics, and public-only telemetry across multiple runtimes.

Current status:

- Implemented today: CLI lifecycle commands, copy-first projections with opt-in symlink mode, lockfile and SQLite state, history, telemetry consent, `doctor`, `explain`, `validate`, JSON output, `skillctl tui`, and `skillctl mcp serve`.

The product direction is defined in [spec.md](./spec.md). A focused troubleshooting guide lives in [docs/troubleshooting.md](./docs/troubleshooting.md).

## Install

Install the latest published release with the versioned release installer:

```bash
curl -fsSL https://github.com/nibzard/skillctl/releases/latest/download/skillctl-install.sh | sh
```

The curl installer currently supports Linux `x86_64` and macOS (`x86_64` and `arm64`).

To pin a specific release or install into a custom directory:

```bash
curl -fsSL https://github.com/nibzard/skillctl/releases/latest/download/skillctl-install.sh | \
  SKILLCTL_VERSION=v0.1.0 SKILLCTL_INSTALL_DIR="$HOME/.local/bin" sh
```

The release workflow publishes deterministic archives and checksums for Linux `x86_64`, macOS `x86_64`, macOS `arm64`, and Windows `x86_64`. Windows users should download the published zip artifact directly, and Linux `arm64` is not currently published.

## Mental Model

`skillctl` manages five layers of state:

1. Canonical workspace skills live in `.agents/skills/`.
2. Imported skills are pinned in `.agents/skillctl.lock` and cached under `~/.skillctl/store/imports/`.
3. Local customizations for imported skills live in `.agents/overlays/<skill>/`.
4. Effective skills are projected into runtime-visible roots as generated copies by default, or symlinks when explicitly enabled.
5. Local history, pins, rollbacks, projection records, and telemetry consent live in `~/.skillctl/state.db`.

Important rules:

- Generated runtime roots are never the canonical place to edit a skill.
- Imported skills are meant to stay upgradeable until you deliberately `override` or `fork` them.
- Local canonical skills are trusted by default.
- Imported skills start as `imported-unreviewed`.
- If an imported skill contains `scripts/`, `doctor` and `update` surface elevated trust warnings.

## Default Layout

After `skillctl init`, the default workspace looks like this:

```text
repo/
  .agents/
    skills/
    overlays/
    skillctl.yaml
    skillctl.lock
```

The default manifest enables these targets:

- `codex`
- `gemini-cli`
- `opencode`

Add `claude-code`, `github-copilot`, or `amp` in `.agents/skillctl.yaml` when you need them.

## Supported Runtimes

| Runtime | Workspace roots | User roots |
|---|---|---|
| Codex | `.agents/skills` | `~/.agents/skills` |
| Claude Code | `.claude/skills` | `~/.claude/skills` |
| GitHub Copilot | `.github/skills` or `.claude/skills` | `~/.copilot/skills` or `~/.claude/skills` |
| Gemini CLI | `.agents/skills` or `.gemini/skills` | `~/.agents/skills` or `~/.gemini/skills` |
| Amp | `.agents/skills` | `~/.config/agents/skills` or `~/.config/amp/skills` |
| OpenCode | `.agents/skills`, `.claude/skills`, or `.opencode/skills` | `~/.agents/skills`, `~/.claude/skills`, or `~/.config/opencode/skills` |

Projection planning follows the manifest policy:

- `prefer-neutral`: prefer shared neutral roots such as `.agents/skills` where documented
- `prefer-native`: prefer vendor-native roots such as `.claude/skills` or `.opencode/skills`
- `minimize-noise`: choose the fewest documented compatible roots

## Quickstart

Create the workspace and project the default targets:

```bash
skillctl init
skillctl sync
```

Install a skill from a local repo or directory:

```bash
skillctl install ../shared-skills --interactive
```

Install from Git in non-interactive mode:

```bash
skillctl install https://github.com/acme/skills.git --name ai-sdk --scope workspace
```

Inspect what actually won and where it projects:

```bash
skillctl explain ai-sdk
skillctl path ai-sdk
skillctl doctor
```

## Command Guide

Core lifecycle commands:

- `skillctl init`: create `.agents/`, a minimal manifest, a lockfile, and default Git excludes.
- `skillctl install <source>` or `skillctl i <source>`: detect candidates from Git URLs, local paths, or local archives and install an exact pinned revision.
- `skillctl sync`: rebuild generated runtime-visible copies from the current effective graph.
- `skillctl update [skill]`: check upstream state, detect overlays or drift, and recommend a safe next action.
- `skillctl pin <skill> <ref>`: move a managed import to a specific revision.
- `skillctl rollback <skill> <version-or-commit>`: restore a previously recorded version or commit.
- `skillctl remove <skill>`: remove managed install state and generated projections for a skill.

Inspection and recovery commands:

- `skillctl list`: show managed installs with scope, source, drift counters, and projections.
- `skillctl history [skill]`: show installs, update checks, projections, rollbacks, overlays, forks, cleanup, and telemetry consent changes.
- `skillctl explain <skill>`: show the active winner, shadowed candidates, target visibility, and drift.
- `skillctl path <skill>`: show canonical, cached, overlay, planned, and projected filesystem paths.
- `skillctl validate`: check manifest, lockfile, skill, and overlay correctness.
- `skillctl doctor`: diagnose missing, stale, shadowed, detached, or conflicting skills.
- `skillctl clean`: remove only `skillctl`-generated projections and unused generated state.

Customization commands:

- `skillctl override <skill>`: create or reuse `.agents/overlays/<skill>/` for targeted file overrides.
- `skillctl fork <skill>`: copy the current effective imported skill into `.agents/skills/<skill>/` and detach it from upstream lifecycle management.
- `skillctl enable <skill>` / `skillctl disable <skill>`: keep or remove a managed import from the active graph without deleting its history.

Telemetry commands:

- `skillctl telemetry status`
- `skillctl telemetry enable`
- `skillctl telemetry disable`

Additional surfaces:

- `skillctl tui`: read-only terminal dashboard for inspection, update context, and history; opening it does not bootstrap bundled skills or write new history entries
- `skillctl mcp serve`: stdio MCP bridge with v1 tool parity

## Common Workflows

Install a public skill and inspect the result:

```bash
skillctl install https://github.com/acme/skills.git --name ai-sdk --scope workspace
skillctl explain ai-sdk
skillctl doctor
```

Customize an imported skill without forking it:

```bash
skillctl override ai-sdk
$EDITOR .agents/overlays/ai-sdk/SKILL.md
skillctl sync
skillctl update ai-sdk
```

Pin or roll back a managed import:

```bash
skillctl pin ai-sdk main
skillctl history ai-sdk
skillctl rollback ai-sdk 0123456789abcdef
```

Use stable JSON output for agents or scripts:

```bash
skillctl list --json
skillctl update ai-sdk --json
skillctl explain ai-sdk --json
skillctl doctor --json
```

MCP v1 tools:

- `skills_list`
- `skills_install`
- `skills_remove`
- `skills_sync`
- `skills_update`
- `skills_rollback`
- `skills_history`
- `skills_explain`
- `skills_override_create`
- `skills_validate`
- `skills_doctor`
- `skills_telemetry_status`

TUI action equivalents:

- Installed skills: `skillctl list`
- Update state: `skillctl update [skill]`
- Visibility and drift: `skillctl explain <skill>`
- Filesystem roots: `skillctl path <skill>`
- Rollback context: `skillctl history [skill]`

## Troubleshooting

Start with these commands:

```bash
skillctl doctor
skillctl explain <skill>
skillctl path <skill>
skillctl history <skill>
```

Common problem patterns:

- Missing skill in one runtime: `doctor` plus `explain --target <runtime>` shows the planned root, active winner, and whether the import is disabled or missing.
- Stale generated copy: `doctor` reports `projection-drift` or `wrong-precedence-root`; run `skillctl sync`.
- Shadowed or conflicting skill: `explain` shows the winner and shadowed candidates; resolve the same-name conflict or remove one source.
- Detached import: `update` reports `detached`; use `fork` only when you want full local ownership.
- Broken overlay: `doctor` reports `missing-overlay-root`, `invalid-overlay-path`, or `invalid-overlay-mapping`.

See [docs/troubleshooting.md](./docs/troubleshooting.md) for the full guide.

## JSON And Exit Codes

Every command supports `--json` and uses the same response envelope:

```json
{
  "ok": true,
  "command": "update",
  "warnings": [],
  "errors": [],
  "data": {}
}
```

Process exit codes:

- `0`: success
- `1`: operational error
- `2`: success with warnings
- `3`: validation or conflict failure
- `4`: trust gate blocked
- `5`: interactive input required

## Telemetry

Telemetry is:

- enabled by default after a first-run notice,
- public-only,
- content-free,
- immediately opt-outable with `skillctl telemetry disable`.

Remote telemetry never includes private repository identifiers, skill contents, arbitrary diffs, or private local paths. Local history remains available even when telemetry is disabled.

## Development

CI enforces the same local checks that keep the Rust codebase clean:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
```

The library crate also denies missing public API documentation and broken intra-doc links, so doc coverage regressions fail the docs gate instead of drifting silently.
