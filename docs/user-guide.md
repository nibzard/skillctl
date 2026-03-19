# skillctl User Guide

`skillctl` is a local-first manager for the open `SKILL.md` ecosystem. It gives you one place to install, inspect, customize, and maintain skills across the agent runtimes you actually use.

This guide starts with a quickstart, then expands into the longer day-to-day workflows:

- creating a workspace
- installing skills from Git, local directories, or archives
- understanding what is active and where it projects
- customizing imported skills safely
- updating, pinning, rolling back, disabling, forking, and removing skills
- diagnosing drift, conflicts, and trust warnings

For problem-focused fixes, see [troubleshooting.md](./troubleshooting.md).

## Quickstart

This quickstart assumes `skillctl` is already installed and available on your `PATH`.

Create or enter a repository, then initialize the default layout:

```bash
skillctl init
skillctl sync
```

That creates:

```text
.agents/skills/
.agents/overlays/
.agents/skillctl.yaml
.agents/skillctl.lock
```

Install a skill from a Git repository:

```bash
skillctl install https://github.com/acme/skills.git --name ai-sdk --scope workspace
```

Or install from a local directory and let `skillctl` prompt you when needed:

```bash
skillctl install ../shared-skills --interactive
```

Inspect what actually won:

```bash
skillctl explain ai-sdk
skillctl path ai-sdk
skillctl doctor
```

Customize an imported skill without forking the whole thing:

```bash
skillctl override ai-sdk
$EDITOR .agents/overlays/ai-sdk/SKILL.md
skillctl sync
```

Later, check for upstream changes and manage the version you want:

```bash
skillctl update ai-sdk
skillctl pin ai-sdk main
skillctl rollback ai-sdk <version-or-commit>
```

If you only remember four commands, remember these:

```bash
skillctl doctor
skillctl explain <skill>
skillctl path <skill>
skillctl history <skill>
```

## The Mental Model

`skillctl` is easiest to use once you separate four different kinds of state:

1. Canonical local skills live in `.agents/skills/`. These are first-party skills you own directly.
2. Managed imports are third-party skills you install from Git, a local directory, or an archive. `skillctl` records them in the manifest and lockfile, then stores cached source snapshots under `~/.skillctl/store/imports/`.
3. Overlays live in `.agents/overlays/<skill>/`. They replace matching upstream files without copying the entire imported skill.
4. Generated projections are the runtime-visible copies or links placed where agent tools actually look for skills.

The most important rule in the system is simple:

- edit canonical skills or overlays, not generated runtime copies

Generated copies are outputs. If you edit them directly, `doctor` and `update` may report drift and recommend a safer workflow.

## Workspace Layout And Local State

After `skillctl init`, your repository stores the workspace-facing pieces:

```text
repo/
  .agents/
    skills/
    overlays/
    skillctl.yaml
    skillctl.lock
```

Outside the repository, `skillctl` keeps machine state in your home directory:

- `~/.skillctl/state.db`: the local SQLite state store for installs, projections, update checks, pins, rollbacks, local modifications, history, and telemetry consent
- `~/.skillctl/store/imports/`: cached imported source trees, namespaced by scope and workspace

The repository files answer different questions:

- `.agents/skillctl.yaml`: what the workspace intends to use
- `.agents/skillctl.lock`: the exact imported revisions and hashes currently pinned
- `~/.skillctl/state.db`: what `skillctl` has observed and recorded locally over time

## Scopes, Targets, And Projection Roots

`skillctl` can manage skills in two scopes:

- `workspace`: the skill belongs to the current repository
- `user`: the skill is installed into your home directory for general use

Workspace scope is the default for project-specific dependencies. User scope is useful for always-on utility skills.

`skillctl` also plans projections for the runtimes you enable. The current runtime set includes:

- `codex`
- `claude-code`
- `github-copilot`
- `gemini-cli`
- `amp`
- `opencode`

The default workspace targets are:

- `codex`
- `gemini-cli`
- `opencode`

The planner decides where each runtime should see the active skill. `skillctl path <skill>` shows the exact planned and projected roots, and `skillctl explain <skill>` shows whether a runtime can currently see that skill.

## Installing Skills

`skillctl install <source>` accepts:

- a Git URL
- a local directory
- a local archive

Examples:

```bash
skillctl install https://github.com/acme/skills.git --name ai-sdk --scope workspace
skillctl install ../shared-skills --interactive
skillctl install ./skills.tar.gz --name release-notes --scope user
```

What happens during install:

1. `skillctl` inspects the source and finds skill candidates.
2. You select the exact skill name and scope, either explicitly or interactively.
3. `skillctl` updates the manifest and lockfile.
4. It stores a cached immutable copy of the imported source under `~/.skillctl/store/imports/`.
5. It materializes projections into the enabled runtime roots.
6. It records install and projection state in the local state store.

Non-interactive mode never guesses. If the source exposes multiple candidates, or the exact name is ambiguous, you must tell `skillctl` exactly what you want:

```bash
skillctl install <source> --name <skill> --scope workspace
```

If you prefer a prompt:

```bash
skillctl install <source> --interactive
```

## What To Run After Install

These commands answer different questions after a successful install:

```bash
skillctl list
skillctl explain <skill>
skillctl path <skill>
skillctl doctor
```

Use them like this:

- `list`: inventory of managed installs, projections, scope, pinned revision, and current state
- `explain`: which candidate won for a projected skill name, why it won, and which runtimes can see it
- `path`: where the canonical root, cached import root, overlay root, planned roots, and generated projections live
- `doctor`: whether anything is stale, shadowed, conflicting, detached, or otherwise unhealthy

When behavior is unclear, `explain` and `path` are usually the fastest way to restore confidence.

## Understanding Winner Selection

One projected skill name may have multiple candidates:

- a canonical workspace skill in `.agents/skills/`
- a managed imported skill
- an imported skill with a managed overlay
- a detached or forked local copy

`skillctl explain <skill>` shows:

- the current winner
- shadowed candidates that lost
- same-name conflicts when a single winner cannot be chosen
- target visibility for the enabled runtimes

When two things seem to define the same skill, start there.

## Syncing Projections

`skillctl sync` recomputes the effective-skill graph and refreshes generated runtime-visible copies:

```bash
skillctl sync
skillctl --scope workspace --target codex --target gemini-cli sync
```

Use it when:

- you changed `.agents/skillctl.yaml`
- you added or removed local canonical skills
- you edited an overlay
- a runtime still sees stale generated content

`sync` only refreshes the directories previously managed by `skillctl`. It does not blindly rewrite unrelated runtime directories.

## Customizing Imported Skills With Overlays

When you want a small local change but still want to keep the skill upgradeable, use an overlay:

```bash
skillctl override ai-sdk
```

That creates or reuses:

```text
.agents/overlays/ai-sdk/
```

An overlay replaces matching upstream files without copying the entire imported source. This is the preferred workflow when:

- you want to tweak text, prompts, or metadata
- you want a managed customization layer
- you still want `update`, `pin`, and `rollback` to keep working

Typical workflow:

```bash
skillctl override ai-sdk
$EDITOR .agents/overlays/ai-sdk/SKILL.md
skillctl sync
skillctl explain ai-sdk
```

Important overlay rules:

- overlay paths must be portable relative paths
- overlay files must map to files that exist in the imported skill
- overlays are for imported skills, not for canonical local skills you already own

If something looks wrong, run:

```bash
skillctl path ai-sdk
skillctl doctor
```

## Updating Imported Skills

`skillctl update` checks an imported skill against its upstream source and recommends the next safe action:

```bash
skillctl update
skillctl update ai-sdk
skillctl update ai-sdk --json
```

The current implementation is intentionally conservative. It reports plans and follow-up actions instead of overwriting local changes blindly.

You may see outcomes like:

- `apply`: safe to refresh the pinned revision
- `create-overlay`: local changes should move into `.agents/overlays/<skill>`
- `detach`: keep a full local copy instead of continuing as a managed import
- `skip`: keep the current pin for now

When `update` reports drift or trust warnings, inspect more context:

```bash
skillctl explain <skill>
skillctl history <skill>
skillctl doctor
```

## Pinning And Rolling Back

Managed imports are recorded at exact revisions. You can control that lifecycle explicitly.

Pin a skill to a branch, tag, or commit:

```bash
skillctl pin ai-sdk main
skillctl pin ai-sdk 0123456789abcdef
```

This updates the manifest, lockfile, cached import source, install record, and projections so future updates compare against a stable baseline.

Roll back to a previously recorded version or effective revision:

```bash
skillctl rollback ai-sdk 0123456789abcdef
skillctl rollback ai-sdk sha256:effective-version
```

To find a good rollback point:

```bash
skillctl history ai-sdk
```

## Forking Into Full Local Ownership

When a managed import has diverged enough that you want to own it entirely, use:

```bash
skillctl fork ai-sdk
```

Forking:

- copies the current effective skill into `.agents/skills/<skill>/`
- merges any overlay content into that local copy
- removes managed import state for that skill
- marks the skill as detached from upstream lifecycle management

Choose `fork` when:

- an overlay is no longer enough
- you want full local ownership
- you no longer want the imported upgrade flow

Choose `override` when:

- you only need a small managed customization layer
- you still want to track and update upstream cleanly

## Enabling, Disabling, Removing, And Cleaning

These commands solve different lifecycle problems.

Disable a managed import without deleting its cached source or history:

```bash
skillctl disable ai-sdk
skillctl enable ai-sdk
```

Disabled imports stay on disk but stop participating in resolution and projection until re-enabled.

Remove a managed skill entirely:

```bash
skillctl remove ai-sdk
skillctl --scope user remove skillctl
```

Remove deletes managed imports, cached stored sources, and generated projections for the selected skill. It does not delete canonical local skills, and it generally preserves overlays unless the detached local copy itself is being removed.

Clean only generated or unused managed state:

```bash
skillctl clean
```

`clean` removes stale skillctl-generated projections and no-longer-needed cached state. It does not delete canonical local skills or overlays just because they exist.

## Structural Validation And Runtime Diagnostics

There are two commands that sound similar but answer different questions:

```bash
skillctl validate
skillctl doctor
```

Use `validate` when you want structural correctness:

- manifest syntax and schema
- lockfile shape
- skill directory validity
- overlay mapping correctness

Use `doctor` when you want runtime-facing diagnostics:

- missing, stale, or shadowed skills
- projection drift
- wrong precedence roots
- trust warnings
- detached or forked state
- missing cached import state

When you are unsure which one to run, start with `doctor`.

## History And Local Ledger

`skillctl` records a local append-only history in `~/.skillctl/state.db`.

Inspect it with:

```bash
skillctl history
skillctl history ai-sdk
skillctl history ai-sdk --json
```

History entries can include:

- installs
- update checks
- projections
- pins
- rollbacks
- overlays
- forks
- cleanup events
- telemetry consent changes

If a skill behaves differently than you expected, `history` usually tells you when the change happened.

## JSON Output, TUI, And MCP

Every CLI command supports `--json`, which returns the normal response envelope:

```bash
skillctl --json list
skillctl --json path ai-sdk
skillctl --json doctor
```

This is the best mode for scripts, editors, and tooling.

For human inspection, `skillctl tui` opens a read-only dashboard:

```bash
skillctl tui
skillctl --scope workspace --name ai-sdk tui
skillctl --target codex tui
```

The TUI is for inspection only. Opening it does not bootstrap bundled skills or write new history entries.

For agent integrations, `skillctl mcp serve` exposes the same lifecycle surface over MCP using the same JSON contract:

```bash
skillctl mcp serve
```

The shipped v1 tools mirror the CLI closely, such as `skills_list`, `skills_install`, `skills_update`, `skills_history`, `skills_override_create`, and `skills_doctor`.

## Telemetry

Telemetry is local-first and conservative by design.

Key points:

- local history is always kept
- remote telemetry is enabled by default only after a first-run notice
- only public-source install and update events are eligible
- skill contents, private repository identifiers, and arbitrary local paths are not sent

Inspect or change consent with:

```bash
skillctl telemetry status
skillctl telemetry disable
skillctl telemetry enable
```

Disabling telemetry does not disable local history, pins, diagnostics, or the rest of the product.

## A Typical Day-To-Day Workflow

A practical workflow for a team repository looks like this:

1. Keep first-party, owned skills in `.agents/skills/`.
2. Install third-party skills with `skillctl install`.
3. Run `skillctl sync` when targets or overlays change.
4. Use `skillctl explain`, `skillctl path`, and `skillctl doctor` when the active result is unclear.
5. Use `skillctl override` for small customizations.
6. Use `skillctl update` to inspect upstream changes before refreshing.
7. Use `skillctl pin` when you want to lock a known-good revision.
8. Use `skillctl rollback` when you need to return to a prior known-good version.
9. Use `skillctl fork` when you want full local ownership.

## Start Here When Something Looks Wrong

When a skill is missing, stale, or blocked:

```bash
skillctl doctor
skillctl explain <skill>
skillctl path <skill>
skillctl history <skill>
```

Common interpretations:

- a runtime cannot see a skill: check `explain` and `path`
- a generated copy looks stale: run `sync`
- an update is blocked: inspect `doctor`, `update`, and `history`
- two same-name skills are competing: inspect `explain`
- an overlay is invalid: inspect `doctor` and the overlay root

For deeper, issue-by-issue guidance, use [troubleshooting.md](./troubleshooting.md).
