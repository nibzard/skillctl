Test-driven development is mandatory: write the test first, then write the implementation.

# skillctl - specification.md

Status: Draft v0.2  
Date: 2026-03-19  
Audience: product, design, CLI, platform, and agent-integration engineers

## Engineering principles for the Rust implementation

These principles should appear at the top of the spec because they constrain how the product is built, not just what it does.

### Rust engineering best practices

1. Domain-driven module boundaries  
   Organize the codebase by product domains such as manifest, install, update, overlay, projection, history, telemetry, and diagnostics, not by generic technical layers alone.

2. Small, explicit modules  
   Prefer small modules with narrow responsibilities, explicit inputs and outputs, and minimal shared mutable state.

3. Strong type boundaries  
   Use Rust's type system to model domain concepts explicitly, especially for skill identity, scope, target, revision, trust state, and install status. Avoid stringly typed core logic.

4. Clear separation of pure logic and side effects  
   Keep planning, resolution, and validation logic pure where possible. Isolate filesystem, Git, network, terminal, and telemetry side effects behind clear interfaces.

5. Documentation-first public APIs  
   All public modules, structs, enums, traits, and important functions should have Rust doc comments. Complex flows should include short module-level docs explaining invariants and ownership.

6. No orphaned or dead code  
   The project should not accumulate unused modules, stale feature flags, commented-out code paths, or abandoned compatibility shims. Remove dead code instead of preserving it "just in case."

7. Strict lint and formatting discipline  
   The codebase should pass `cargo fmt`, `clippy` with a strict baseline, and documentation checks in CI. Warnings should be treated as defects, not background noise.

8. Testable architecture  
   Core planning, conflict resolution, overlays, history recording, and update logic should be unit-testable without touching the real filesystem or network.

9. Deterministic behavior by default  
   Filesystem writes, lockfile generation, history events, and JSON output should be deterministic so the tool is reliable in CI and predictable for agents.

10. Explicit error design  
   Errors should be structured, contextual, and actionable. Avoid opaque `anyhow`-style top-level blobs in domain boundaries; preserve typed errors until the CLI presentation layer.

11. Backward-compatible state evolution  
   Manifest, lockfile, and local database changes should be versioned and migrated deliberately. Never rely on silent state breakage across releases.

12. Minimal macro and async complexity  
   Prefer straightforward Rust over clever macro-heavy or over-abstracted designs. Use async only where it materially improves the product, not as a default style.

### Engineering checklist

- TDD is required: every feature and bug fix starts with a failing test.
- Write unit tests before implementing domain logic.
- Add integration tests before wiring filesystem, Git, network, or CLI flows.
- Do not merge behavior changes without tests unless the change is purely non-functional documentation or formatting.
- Keep modules domain-oriented and small enough to understand without cross-referencing the whole codebase.
- Add Rust doc comments to all public modules, types, and important functions.
- Add module-level docs where invariants, ownership, or state transitions are non-obvious.
- Remove dead code, stale flags, and commented-out implementations instead of leaving them behind.
- Keep domain logic strongly typed; avoid stringly typed identifiers in core flows.
- Isolate side effects behind explicit interfaces so planning and validation logic stay testable.
- Keep lockfile writes, history events, and JSON output deterministic.
- Treat warnings as defects and keep the codebase clean under strict lint settings.
- Require `cargo fmt --check` in CI.
- Require `cargo clippy --all-targets --all-features -- -D warnings` in CI.
- Require `cargo test` in CI.
- Require `cargo doc --no-deps` or equivalent documentation verification in CI.
- Version and migrate manifest, lockfile, and database schema changes explicitly.
- Prefer simple Rust over macro-heavy abstractions unless the abstraction clearly improves correctness or maintainability.

## 1. Executive summary

`skillctl` is a local-first, cross-agent skill manager for the open `SKILL.md` ecosystem.

V1 is centered on two primary features:

1. skill installation and update lifecycle management
2. telemetry and local history for installed skills

The ecosystem is converging on an open skill format, but discovery roots, packaging, installation, updates, local customization, and diagnostics are still fragmented. That fragmentation creates duplicated copies, noisy repositories, brittle symlink setups, unclear active versions, and no trustworthy record of what changed.

`skillctl` gives users and agents one operating model:

1. keep canonical workspace skills in a standard dot-directory,
2. install public skills from Git or local sources,
3. pin imported skills to a commit,
4. apply overlays without forking everything,
5. project effective copies into runtime-visible locations,
6. explain why a skill is present, missing, stale, or shadowed,
7. keep a local version history and optional telemetry trail.

This product is not a new `SKILL.md` format, not a hosted marketplace in v1, and not a generalized harness manager. It is the missing control plane for the local lifecycle of agent skills.

## 2. Problem statement

Advanced users working across Codex, Claude Code, GitHub Copilot, Gemini CLI, Amp, OpenCode, and related tools face six recurring problems.

### 2.1 Discovery path fragmentation

Different agents discover skills from different paths and scopes. Some support vendor-specific directories, some support neutral aliases such as `.agents/skills`, and some support multiple compatible roots with different precedence rules.

Result: users duplicate the same skill across `.agents/skills`, `.claude/skills`, `.github/skills`, `.opencode/skills`, `~/.agents/skills`, `~/.claude/skills`, `~/.config/agents/skills`, `~/.config/opencode/skills`, repo-root `skills/`, and tool-specific locations.

### 2.2 Packaging vs runtime mismatch

Some ecosystems package or publish skills from repo-root `skills/`, while many runtimes actually load from dot-directories such as `.agents/skills`, `.claude/skills`, or `.opencode/skills`.

Result: the same skill often exists twice in one repo: once for packaging or discovery, once for runtime loading.

### 2.3 Symlink workflows are unreliable

Symlinks are attractive because they imply one canonical copy, but real-world support is inconsistent. Even where docs mention symlinks, users still hit bugs around directory symlinks, relative symlinks, and symlinked `SKILL.md` files.

Result: brittle setups, cross-platform failures, and low trust in the toolchain.

### 2.4 No real local versioning model

`metadata.version` in `SKILL.md` is informative, not lifecycle semantics. Users still need to:

- install a skill from a repo,
- pin it to a commit,
- update it later,
- keep local modifications,
- understand drift from upstream,
- recover or backtrack if an update goes wrong.

Result: users either fork everything or edit copied runtime files directly.

### 2.5 No system-of-record for local changes

Even when a user only changes one installed skill, there is often no durable record of:

- what was installed,
- which commit it came from,
- what overlay was applied,
- whether the runtime copy was modified by hand,
- which version is currently active.

Result: updates are risky and debugging becomes guesswork.

### 2.6 Weak diagnostics and observability

When a skill does not load, users often cannot tell:

- which path the agent scanned,
- which copy won,
- whether a permission or naming rule hid it,
- whether the runtime is reading a different root,
- whether a local edit detached the installed copy from upstream.

Result: noise, failed agent workflows, and support burden.

## 3. Product definition

`skillctl` is a CLI-first skill manager with an optional TUI and MCP server.

It manages the filesystem lifecycle, state, history, diagnostics, and optional telemetry of skills while preserving compatibility with the open Agent Skills standard and vendor-specific extensions.

### 3.1 One-line definition

> `skillctl` is the install, lockfile, overlay, projection, history, telemetry, and diagnostics layer for local agent skills.

### 3.2 Product shape

V1 ships as:

- a small Rust CLI binary,
- a curl-installable distribution path,
- a bundled `skillctl` management skill installed at user scope,
- a workspace manifest,
- a lockfile,
- a local cache and state store,
- a projection engine,
- an adapter system for supported runtimes,
- a diagnostics engine,
- an optional TUI for history and inspection,
- an MCP server exposing the same operations to agents.

### 3.3 Two primary product features

#### Feature A: telemetry

Telemetry exists to answer:

- how skills are installed,
- which public skills are popular,
- whether updates are available or applied,
- how often local modifications block upgrades,
- which doctor failures are most common.

Telemetry must be:

- enabled by default,
- disclosed clearly on first run or install,
- immediately opt-outable,
- limited to public sources,
- content-free,
- safe to disable without losing local history.

#### Feature B: install and update lifecycle

The main user journey is:

1. detect skills from a source,
2. install one or more of them,
3. pin a commit,
4. project compatible copies,
5. detect upstream updates,
6. preserve local changes through overlays or detachment,
7. explain and recover when things drift.

### 3.4 Non-product areas for v1

V1 is not:

- a new `SKILL.md` standard,
- a hosted registry,
- a cloud sync service,
- a GUI desktop app,
- a runtime replacement for Codex, Claude Code, GitHub Copilot, Gemini CLI, Amp, or OpenCode,
- a script sandbox or execution environment,
- a generalized manager for all harness artifacts such as `AGENTS.md`, hooks, subagents, and MCP configs, though the architecture should not block that future.

## 4. Design goals

### 4.1 Primary goals

1. Compatibility first  
   Preserve the open `SKILL.md` ecosystem and documented vendor-specific directories.

2. Dot-directory first  
   Keep workspace skill state under `.agents/` by default instead of spreading control files across the repo root.

3. One canonical workspace standard  
   Prefer `.agents/skills` as the default workspace authoring and installation root where the target supports it.

4. Copy-first reliability  
   Use copies by default and treat symlinks as an opt-in optimization.

5. Safe install and update lifecycle  
   Imported skills must remain updatable without destroying local changes.

6. Local system of record  
   Every install, update, overlay, local modification, and projection must be traceable.

7. Humans and agents use the same system  
   Every important CLI operation must also be available as machine-readable non-interactive output and through MCP.

8. Explainability  
   The tool must always be able to answer: "why is this skill here, missing, shadowed, stale, detached, or not loading?"

9. Minimal, privacy-preserving telemetry  
   Telemetry should improve the product without collecting private repo data or skill content.

### 4.2 Secondary goals

- fast local execution,
- cross-platform behavior,
- deterministic machine-readable output,
- Git-friendly behavior,
- CI-friendly behavior,
- script-risk visibility,
- a good terminal-native inspection workflow.

## 5. Non-goals

V1 will not:

- rewrite skill contents to invent a new naming scheme,
- mutate upstream `SKILL.md` semantics unless explicitly requested,
- auto-resolve all name conflicts by renaming skills,
- guarantee identical behavior across all runtimes,
- auto-execute bundled scripts,
- upload private repositories or private skill contents for telemetry,
- require a hosted account to use core lifecycle features.

## 6. Core principles

### 6.1 Keep the standard, manage the lifecycle

`skillctl` does not compete with the Agent Skills spec. It manages what the spec leaves open: install, pinning, overlays, projection, history, cleanup, and diagnostics.

### 6.2 Treat `.agents/` as the neutral workspace home

For workspace-local skill management, the preferred standard home is:

- `.agents/skills/` for canonical workspace skills and installed neutral skills,
- `.agents/overlays/` for local shadow-file overlays,
- `.agents/skillctl.yaml` for manifest,
- `.agents/skillctl.lock` for lockfile.

Repo-root `skills/` may still be detected and imported, but it is not the default workspace control plane.

### 6.3 Prefer the fewest physical roots that remain documented

When multiple agents support the same documented directory, `skillctl` should prefer the fewest physical roots needed to satisfy enabled targets.

### 6.4 Keep overlays separate from projections

Users must never be forced to edit generated runtime folders or vendored copies directly. Overlays belong in a dedicated overlay root, not inside runtime-generated skill directories.

### 6.5 Keep a durable local history

Even if telemetry is disabled, `skillctl` must still preserve a local install and update history so users and agents can reason about current and past state.

### 6.6 Telemetry is opt-out, public-only, and minimal

Telemetry is enabled by default only with a first-run notice and a one-step opt-out path. It must never include private repo identifiers, private skill content, or arbitrary file diffs.

### 6.7 All interactive features must have non-interactive equivalents

If the TUI or an interactive prompt can perform an action, the same action must also be available through a stable CLI command and JSON output so agents can use it.

### 6.8 Rust is the implementation target

The implementation language for `skillctl` is Rust, not Go.

### 6.9 Ship the control plane with its own skill

`skillctl` should install a bundled `skillctl` skill into the preferred user-scope root for supported agents so the agent can:

- inspect installed skills,
- diagnose missing skills,
- check update state,
- guide the user through install, rollback, pin, and cleanup flows.

This bundled skill should be available by default across repositories unless a runtime's own precedence rules place a higher-priority scope ahead of user scope.

## 7. Primary user stories

### 7.1 Solo multi-agent user

"I use Codex, Gemini CLI, OpenCode, and Amp in the same repo. I want one command to install a skill once and keep compatible runtimes in sync."

### 7.2 Team maintaining workspace-local skills

"We want our project skills committed under a standard dot-directory, not scattered across repo-root folders and vendor-specific copies."

### 7.3 User installing a public skill

"I want `skillctl install` to detect skills in a repo, let me choose one interactively, and pin the exact commit that was installed."

### 7.4 User updating a modified installed skill

"I changed an installed skill locally. When upstream changes, I want the tool to detect that, explain the conflict, and tell me whether to overlay, detach, or publish my variant."

### 7.5 Agent self-maintenance workflow

"My agent should be able to check installed skills, inspect history, detect updates, and use `doctor` without manually editing dot-directories."

### 7.6 Debugging workflow

"A skill is not loading in one agent. I want one command that tells me the active root, precedence, permissions, name collisions, and exact reason it is missing."

## 8. Concept model

### 8.1 Skill

A directory containing `SKILL.md` and optional supporting files.

### 8.2 Canonical workspace skill

A skill directly authored or maintained in `.agents/skills/`.

### 8.3 Imported skill

A skill sourced from Git, a local path, or an archive and pinned by lockfile.

### 8.4 Overlay

A local shadow-file layer in `.agents/overlays/<skill-id>/` that replaces selected upstream files without forking the full skill.

### 8.5 Projection

A materialized copy or symlink of an effective skill into a runtime-visible directory.

### 8.6 Installed copy

A managed effective skill version currently deployed to a workspace or user scope.

### 8.7 Installation record

The metadata `skillctl` stores for an installed skill, including source, pinned commit, current effective version, and modification state.

### 8.8 History ledger

A local event trail recording installs, updates, projections, local change detection, detachments, and cleanup operations.

### 8.9 Target

A supported agent runtime, such as `codex`, `claude-code`, `github-copilot`, `gemini-cli`, `amp`, or `opencode`.

### 8.10 Physical root

An actual directory on disk that a runtime scans, for example `.agents/skills` or `.claude/skills`.

### 8.11 Scope

At minimum in v1:

- `workspace`
- `user`

Future:

- `admin`
- `org`
- `plugin`

### 8.12 Effective skill

The final resolved view of a skill after applying source, overlay, enablement, and target-specific filtering.

## 9. System architecture

```text
sources + imports + overlays
            |
            v
      resolver graph
            |
            v
   target compatibility lint
            |
            v
  projection planner + installer
            |
            v
 materializer (copy default)
            |
            +--> local history ledger
            +--> doctor / explain
            +--> optional telemetry emitter
            +--> MCP server
            +--> TUI
```

### 9.1 Main components

1. manifest loader
2. lockfile loader
3. source detector and fetcher
4. skill parser and validator
5. overlay resolver
6. conflict resolver
7. adapter layer
8. install and update planner
9. projection materializer
10. history store
11. diagnostics engine
12. telemetry emitter
13. MCP bridge
14. TUI application

## 10. Filesystem layout and state

### 10.1 Recommended workspace layout

```text
repo/
  .agents/
    skills/                    # canonical workspace skills and neutral installed skills
      release-notes/
        SKILL.md
    overlays/                  # local overlays for imported skills
      ai-sdk/
        SKILL.md
    skillctl.yaml              # manifest
    skillctl.lock              # lockfile

  .claude/skills/             # generated only when needed
  .github/skills/             # generated only when needed
  .opencode/skills/           # generated only when needed by policy
```

### 10.2 Local user-level state

```text
~/.skillctl/
  state.db                    # system of record
  store/                      # cached imports
  logs/
  telemetry/
  projections/
  backups/
```

### 10.3 Source-of-truth rules

- `.agents/skills/` is the default workspace authoring and neutral installation location.
- `.agents/overlays/` is the default overlay location.
- `.agents/skillctl.yaml` and `.agents/skillctl.lock` are the default workspace control files.
- `~/.skillctl/` is always generated local state and the authoritative system of record for history.
- generated runtime directories are never the canonical edit location.
- repo-root `skills/` may be detected as an import source but is not the preferred default workspace layout.

### 10.4 Git behavior

Default behavior:

- `.agents/skills/`, `.agents/overlays/`, `.agents/skillctl.yaml`, and `.agents/skillctl.lock` are committable.
- `~/.skillctl/` is outside the repo and never committed by default.
- generated runtime roots are excluded from Git using `.git/info/exclude` by default, not by mutating `.gitignore`.

Optional behavior:

- a team may opt into committing generated runtime roots,
- a team may opt into keeping overlays private outside the repo.

## 11. Manifest

V1 uses `.agents/skillctl.yaml`.

### 11.1 Manifest goals

- short,
- diff-friendly,
- readable,
- optional for the simplest local-only flow,
- easy to emit as JSON for agents.

### 11.2 Minimal manifest

```yaml
version: 1

targets:
  - codex
  - gemini-cli
  - opencode
```

Semantics:

- local canonical skills are read from `.agents/skills/` automatically,
- projection defaults apply,
- telemetry defaults apply,
- no external imports yet.

### 11.3 Full manifest example

```yaml
version: 1

projection:
  policy: prefer-neutral       # minimize-noise | prefer-neutral | prefer-native
  mode: copy                   # copy | symlink
  prune: true
  git_exclude: local           # local | gitignore | none

layout:
  skills_dir: .agents/skills
  overlays_dir: .agents/overlays

imports:
  - id: ai-sdk
    type: git
    url: https://github.com/vercel/ai.git
    ref: main
    path: skills/ai-sdk
    scope: workspace
    enabled: true

overrides:
  ai-sdk: .agents/overlays/ai-sdk

targets:
  - codex
  - gemini-cli
  - amp
  - opencode

telemetry:
  enabled: true
  mode: public-only            # public-only | off

adapters:
  codex:
    workspace_root: auto
    user_root: auto
  opencode:
    workspace_root: auto
    user_root: auto
```

### 11.4 Manifest semantics

#### `projection.policy`

- `minimize-noise`: choose the fewest documented compatible roots.
- `prefer-neutral`: prefer `.agents/skills` where documented and equally compatible.
- `prefer-native`: prefer each runtime's native vendor path.

#### `projection.mode`

- `copy`: default, reliable.
- `symlink`: opt-in, warned when target support is unstable.

#### `imports`

Imported skills are immutable inputs. Local edits happen through overlays or a deliberate detach or fork workflow.

#### `telemetry`

- `enabled: true` means telemetry is permitted after the first-run notice unless the user opts out.
- `mode: public-only` means only public-source events may be emitted.

## 12. Lockfile and local state

### 12.1 Lockfile location

V1 uses `.agents/skillctl.lock`.

### 12.2 Lockfile requirements

The lockfile must pin:

- source type,
- normalized source URL,
- selected subpath,
- resolved commit or digest,
- last observed upstream commit,
- content hash,
- fetched timestamp,
- overlay hash,
- effective version hash,
- first installed timestamp,
- last updated timestamp.

### 12.3 Important rule

`metadata.version` inside `SKILL.md` may be displayed, but it is not trusted as the lifecycle source of truth.

The lifecycle source of truth is the lockfile plus the local state store.

### 12.4 Effective version identity

Every effective skill version is identified by:

```text
source revision + source content hash + overlay hash
```

### 12.5 Local state store

`~/.skillctl/state.db` records:

- install records,
- projection records,
- local modification detection,
- detached or forked state,
- telemetry consent,
- update checks,
- history events,
- pins and rollbacks.

### 12.6 Storage recommendation

Use SQLite for the local system of record.

Rationale:

- excellent fit for local history and queries,
- simple durability model,
- good support in Rust,
- far better than a local key-value design for version history, TUI inspection, and diagnostics.

## 13. Overlay model

### 13.1 Why overlays

Users want to customize imported skills without losing upgradeability.

### 13.2 V1 overlay strategy

Use shadow-file overlays, not patch hunks.

If a file exists in `.agents/overlays/<skill-id>/`, it replaces the same relative file from the imported source.

Advantages:

- simple mental model,
- works for markdown, scripts, and assets,
- easy to diff,
- easy for agents to reason about,
- keeps runtime-visible skill folders clean.

### 13.3 Example

```text
upstream: ~/.skillctl/store/imports/ai-sdk/SKILL.md
overlay:  .agents/overlays/ai-sdk/SKILL.md
result:   effective skill uses the overlay file
```

### 13.4 Why overlays stay outside the skill directory

Keeping overlays outside the runtime skill directory avoids:

- polluting the open skill layout with tool-specific files,
- confusing agents that scan the skill directory,
- coupling editable local state to generated projections.

### 13.5 Full ownership flow

If a user wants full ownership, `skillctl fork <skill>` copies the effective skill into `.agents/skills/` and detaches it from upstream updates.

## 14. Install and update lifecycle

### 14.1 `skillctl install`

Primary command:

```bash
skillctl install <source>
```

Alias:

```bash
skillctl i <source>
```

The command must:

1. inspect the source,
2. detect skills in supported folders,
3. prompt for selection in interactive mode,
4. require an exact skill name in non-interactive mode,
5. pin the installed revision,
6. materialize projections,
7. record the install in the history ledger.

### 14.2 Source detection rules

When a source is a repo or directory, `install` must detect skills in at least:

- `.agents/skills/`
- `.claude/skills/`
- `.opencode/skills/`
- `skills/`

The detected set should be normalized into install candidates with source path, displayed name, and target compatibility hints.

### 14.3 Interactive mode

Interactive mode should:

- list all detected skills,
- show where each was found,
- allow the user to select one or more skills,
- show the pinned commit that will be recorded,
- offer scope selection when needed.

### 14.4 Non-interactive mode

Non-interactive mode must:

- accept `--name <skill-name>` or an equivalent exact selector,
- fail if the name is ambiguous or missing,
- never guess,
- return structured JSON for agents.

### 14.5 Installed version reference

Every installed skill must record the exact commit hash or immutable digest used for that install. That commit becomes the baseline for updates and rollback.

### 14.6 `skillctl update`

`skillctl update [skill]` must:

1. check upstream sources,
2. compare the installed pinned commit to the latest available commit or ref,
3. detect local overlays and direct local modifications,
4. propose an update plan instead of blindly overwriting,
5. record the result in local history,
6. emit a public-only telemetry event if telemetry is enabled.

### 14.7 Local modification handling

If a user changed an installed skill, `update` must explain whether the change is:

- represented by a managed overlay,
- an unmanaged edit to a projected copy,
- a detached local fork.

If the change is unmanaged, `skillctl` should propose one of:

- create an overlay,
- detach into a canonical local skill,
- publish the custom version to a new repo,
- keep the current pinned version and skip the update.

### 14.8 Repo creation helper

If Git and GitHub CLI are installed, `skillctl` may offer an automation path to create a repo for the user's customized skill. This must always be explicit and never automatic.

### 14.9 Rollback and pinning

Users must be able to:

- pin to a specific commit,
- see prior installed versions,
- roll back to a previous effective version,
- keep the latest deployed version as the default unless pinned otherwise.

### 14.10 Bundled `skillctl` skill

On `skillctl` installation, the product should install and keep updated a bundled `skillctl` skill in user scope.

That skill should:

- explain the `skillctl` command surface to agents,
- help agents debug missing or stale skills,
- be treated as a managed built-in asset,
- remain removable only through an explicit user action.

## 15. Adapter model

Adapters define how each runtime is discovered, validated, and projected.

### 15.1 Required adapter capabilities

Each adapter must declare:

- supported scopes,
- documented discovery roots,
- precedence behavior,
- whether neutral shared roots are supported,
- projection compatibility notes,
- frontmatter compatibility notes,
- install-mode risk such as `copy-safe` or `symlink-unstable`,
- extra metadata files understood by the runtime,
- any permissions or loading behaviors relevant to `doctor`.

### 15.2 Initial adapters

V1 must support:

- `codex`
- `claude-code`
- `github-copilot`
- `gemini-cli`
- `amp`
- `opencode`

### 15.3 Default shared-root planner

#### Workspace scope

- `codex` + `gemini-cli` + `opencode` + `amp` -> prefer `.agents/skills`
- `codex` + `gemini-cli` -> prefer `.agents/skills`
- `opencode` alone -> prefer `.agents/skills` under `prefer-neutral`, `.opencode/skills` under `prefer-native`
- `amp` alone -> prefer `.agents/skills`
- `claude-code` + `github-copilot` -> prefer `.claude/skills`
- `github-copilot` alone -> prefer `.github/skills`
- `claude-code` alone -> prefer `.claude/skills`

#### User scope

- `codex` + `gemini-cli` + `opencode` -> prefer `~/.agents/skills`
- `opencode` alone -> prefer `~/.agents/skills` under `prefer-neutral`, `~/.config/opencode/skills` under `prefer-native`
- `amp` alone -> prefer `~/.config/agents/skills`
- `claude-code` + `github-copilot` -> prefer `~/.claude/skills`
- `github-copilot` alone -> prefer `~/.copilot/skills`
- `claude-code` alone -> prefer `~/.claude/skills`

### 15.4 Planner rules

1. Never use undocumented paths.
2. Prefer fewer roots over more roots when compatibility is documented.
3. Prefer neutral paths when equally compatible and equally low-noise.
4. Do not duplicate the same effective skill into multiple roots unless required.
5. Users may override the planner explicitly.

## 16. Conflict resolution

### 16.1 Internal identity

Each skill gets an immutable internal ID such as:

```text
local:workspace:.agents/skills/release-notes
git:https://github.com/vercel/ai.git#skills/ai-sdk
```

### 16.2 Projected identity

Projected identity is still governed by the skill's own `name` field and directory name.

`skillctl` must not rename projected skills by default because that would alter the open standard contract.

### 16.3 Conflict rules

For a given physical root:

- only one effective skill with a given `name` may be projected,
- the winner is chosen by explicit manifest priority first,
- otherwise by source class precedence:
  1. canonical local
  2. overridden imported
  3. imported
- ties are errors.

### 16.4 Explainability

`skillctl explain <skill>` must show:

- effective winner,
- shadowed candidates,
- why the winner won,
- which runtimes will see it,
- which physical root contains it,
- whether the active copy differs from the pinned source.

## 17. Projection and sync algorithm

### 17.1 High-level algorithm

1. load manifest,
2. discover canonical local skills,
3. resolve imports from lockfile and store,
4. apply overlays,
5. validate the core spec,
6. run adapter compatibility lint,
7. resolve name conflicts,
8. compute physical roots,
9. materialize projections,
10. prune stale generated skills,
11. write provenance metadata,
12. write local history events,
13. emit optional telemetry,
14. print a summary.

### 17.2 Projection metadata

Each projected skill directory should include a generated metadata file such as:

```text
.skillctl-projection.json
```

Containing:

- internal ID,
- source,
- resolved revision,
- overlay path,
- projection timestamp,
- tool version,
- physical root,
- generation mode.

This file is for diagnostics only and must not affect runtime behavior.

### 17.3 Prune semantics

`skillctl sync` should remove stale previously-generated projections, but only if they were created by `skillctl`.

Hand-authored directories must never be deleted automatically.

## 18. Symlink policy

### 18.1 Default

Default is `copy`.

### 18.2 Opt-in

`symlink` mode exists for users who explicitly want it.

### 18.3 Risk handling

If a selected target has adapter status `symlink-unstable`, `skillctl` must:

- warn during `sync`,
- return a warning in JSON output,
- recommend copy mode,
- require explicit override.

### 18.4 Practical rule

`skillctl` treats symlinks as an optimization, never as the foundation of its product model.

## 19. Validation and diagnostics

### 19.1 Commands

- `skillctl validate`
- `skillctl doctor`
- `skillctl explain <skill>`

### 19.2 `validate`

Checks:

- `SKILL.md` exists,
- frontmatter parses,
- `name` format and parent-dir match,
- `description` is present,
- no duplicate internal IDs,
- no path traversal in overlays,
- no invalid shadow-file mapping.

### 19.3 `doctor`

Checks everything in `validate` plus:

- projected name conflicts,
- shadowed skills,
- stale lockfile entries,
- missing imports,
- generated roots not matching manifest,
- unsupported adapter-specific fields,
- symlink-mode risk,
- drift between canonical and projection,
- unmanaged direct edits to installed copies,
- target-specific permissions or visibility issues where detectable,
- script-bearing skills from untrusted sources,
- agent-specific loading failures such as wrong path, wrong precedence root, or duplicate names.

### 19.4 `doctor` goals

`doctor` should answer:

- why a target is not loading a skill,
- which root the target is actually reading,
- whether a different copy is winning,
- whether the skill is invalid,
- whether a local edit blocked a clean update.

### 19.5 Output philosophy

Diagnostics must be:

- plain English first,
- machine-readable second,
- action-oriented,
- never vague.

Example:

```text
warning: ai-sdk is not loading in opencode
reason: opencode is reading .agents/skills and found another skill with the same name earlier in precedence
fix: run skillctl explain ai-sdk --target opencode
```

## 20. Security, telemetry, and history

### 20.1 Threat model

Skills may contain scripts, references, and assets. The CLI is a lifecycle manager, not an execution sandbox, but it still must surface risk and avoid oversharing data.

### 20.2 Trust states

V1 trust states:

- `local-trusted`
- `imported-unreviewed`
- `imported-reviewed`

### 20.3 Behavior

- local canonical skills are trusted by default,
- imported skills start as `imported-unreviewed`,
- if a skill contains `scripts/`, `doctor` should show elevated-risk messaging until reviewed.

### 20.4 Local history requirements

The local history ledger must record at least:

- install,
- update check,
- update applied,
- rollback,
- overlay creation,
- direct-modification detection,
- detach or fork,
- cleanup or prune.

### 20.5 Telemetry consent model

Telemetry must:

- show a notice on first run or first install,
- default to enabled,
- allow immediate opt-out,
- remain configurable later with CLI commands,
- never block core functionality if disabled.

### 20.6 Telemetry scope

Remote telemetry may include:

- public repository URL or normalized public source identifier,
- skill identifier,
- command type such as install or update,
- success or failure classification,
- pinned and latest commit identifiers,
- whether local modifications or overlays were detected,
- aggregate popularity metrics.

Remote telemetry must not include:

- private repository URLs or identifiers,
- skill contents,
- arbitrary file diffs,
- prompt content,
- local file paths from private projects.

### 20.7 Public-only rule

If a source is private, local-only, or not confidently known to be public, `skillctl` must keep history locally but suppress remote telemetry for that event.

### 20.8 Backend recommendation

Do not design the telemetry backend around a key-value store as the primary source of truth.

Workers KV or similar systems are acceptable for:

- caching,
- deduplication hints,
- short-lived routing state.

They are a poor primary fit for:

- append-only event history,
- popularity aggregation,
- per-skill update timelines,
- querying modification trends.

Recommended v1 direction:

- a simple ingestion endpoint,
- append-only event storage in a queryable database,
- one of SQLite, libSQL, Postgres, or ClickHouse depending deployment scale.

## 21. CLI specification

### 21.1 Design requirements

The CLI must be:

- memorable,
- low-ceremony,
- scriptable,
- stable for agents,
- quiet by default,
- verbose when asked,
- fully usable without the TUI.

### 21.2 Core commands

```bash
skillctl init
skillctl list
skillctl install <source>
skillctl i <source>
skillctl remove <skill>
skillctl sync
skillctl update [skill]
skillctl pin <skill> <ref>
skillctl rollback <skill> <version-or-commit>
skillctl history [skill]
skillctl doctor
skillctl explain <skill>
skillctl override <skill>
skillctl fork <skill>
skillctl enable <skill>
skillctl disable <skill>
skillctl path <skill>
skillctl validate
skillctl clean
skillctl tui
skillctl telemetry status
skillctl telemetry enable
skillctl telemetry disable
skillctl mcp serve
```

### 21.3 Command semantics

#### `skillctl init`

Create `.agents/skillctl.yaml`, `.agents/skills/`, `.agents/overlays/`, and optional `.git/info/exclude` entries.

#### `skillctl install <source>`

Supported source types in v1:

- Git URL
- local path
- local archive

Examples:

```bash
skillctl install https://github.com/vercel/ai.git --name ai-sdk --scope workspace
skillctl i ../shared-skills --interactive
```

#### `skillctl update [skill]`

Check for updates, show local modifications, propose the next action, and apply or defer explicitly.

#### `skillctl rollback <skill> <version-or-commit>`

Re-activate a previously installed effective version or pinned commit and record the rollback in local history.

#### `skillctl history [skill]`

Show local version and modification history for one skill or all managed skills.

#### `skillctl override <skill>`

Create `.agents/overlays/<skill>/` populated minimally for editing.

#### `skillctl doctor`

Diagnose loading, projection, precedence, and update problems across targets.

#### `skillctl clean`

Remove only `skillctl`-generated projections, stale copies, and generated state. Never delete canonical local skills or overlays without explicit user intent.

#### `skillctl tui`

Open a terminal UI focused on:

- installed versions,
- update availability,
- overlays,
- local modifications,
- target visibility,
- rollback or pin inspection.

Every TUI action must map to a documented CLI command.

### 21.4 Global flags

```text
--json
--quiet
--verbose
--no-input
--interactive
--name <skill>
--scope <workspace|user>
--target <name>
--cwd <path>
```

### 21.5 Exit codes

- `0` success
- `1` operational error
- `2` success with warnings
- `3` validation or conflict failure
- `4` trust gate blocked operation
- `5` interactive input required but `--no-input` was set

### 21.6 JSON output contract

All commands must support:

```json
{
  "ok": true,
  "command": "update",
  "warnings": [],
  "errors": [],
  "data": {}
}
```

Fields must be stable and documented.

## 22. MCP specification

### 22.1 Why MCP

Agents should manage skills through a safe, explicit surface instead of editing dot-directories directly.

### 22.2 MCP tools in v1

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

### 22.3 MCP rules

- every tool must mirror CLI behavior,
- every tool must return structured JSON,
- no hidden mutation outside managed roots,
- destructive actions require explicit parameters,
- interactive-only workflows must have a non-interactive MCP equivalent.

## 23. Runtime compatibility notes

### 23.1 Open standard compatibility

`skillctl` must preserve the open Agent Skills directory contract.

### 23.2 Vendor-specific pass-through

V1 must preserve vendor-specific extras without stripping them, including:

- `agents/openai.yaml`
- Claude-specific frontmatter fields such as `disable-model-invocation`, `user-invocable`, `context`, `agent`, and `hooks`

### 23.3 Linting

If a skill uses fields some enabled targets do not understand, `doctor` should warn but not mutate or remove those fields.

### 23.4 Neutral root strategy

Workspace scope should prefer `.agents/skills` whenever that path is documented for the enabled targets. Vendor-native roots remain necessary where neutral compatibility is unavailable or the user chooses `prefer-native`.

## 24. Recommended implementation choices

These are recommendations, not product requirements.

### 24.1 Language

Recommended: Rust

Reasons:

- small static binary distribution,
- strong cross-platform filesystem support,
- excellent performance for directory walking and hashing,
- mature JSON, YAML, Git, TUI, and SQLite ecosystems,
- good fit for a single-binary CLI plus optional TUI.

### 24.2 Internal modules

Suggested module split:

- `manifest`
- `lockfile`
- `source`
- `skill`
- `overlay`
- `adapter`
- `planner`
- `materialize`
- `history`
- `telemetry`
- `doctor`
- `mcp`
- `tui`

### 24.3 State discipline

No hidden global mutable state outside configured roots and `~/.skillctl/`.

### 24.4 Rust ecosystem guidance

Reasonable implementation choices include:

- `clap` for CLI parsing,
- `serde` and `serde_yaml` for manifest and lockfile IO,
- `rusqlite` or `sqlx` with SQLite for local state,
- `ratatui` plus `crossterm` for the TUI,
- `gix` or shelling to Git only where necessary and explicit.

## 25. Acceptance criteria

### 25.1 Functional acceptance

1. A repo with canonical `.agents/skills/` can be synced into Codex, Gemini CLI, OpenCode, and Amp using `.agents/skills` where documented.
2. A repo with canonical `.agents/skills/` can still project into `.claude/skills` and `.github/skills` where needed.
3. `skillctl install` detects skills in `.agents/skills/`, `.claude/skills/`, `.opencode/skills/`, and `skills/`.
4. Interactive install lets the user choose from detected skills.
5. Non-interactive install requires an exact skill name and never guesses.
6. Imported Git skills are pinned to an exact commit in the lockfile.
7. `skillctl update` detects upstream updates and local modifications before proposing changes.
8. Local overlays survive upstream update.
9. `doctor` explains agent-loading failures deterministically.
10. `history` shows prior installs, updates, and rollbacks.
11. The bundled `skillctl` skill is installed at user scope and remains available to supported agents by default.
12. MCP tools can perform the same lifecycle operations.

### 25.2 Reliability acceptance

1. Copy mode works on macOS, Linux, and Windows.
2. The same repo produces the same projections across fresh runs.
3. Stale generated skills are removed only if previously created by `skillctl`.
4. Manifest-free local flow works with `skillctl init` defaults.
5. Local history remains intact even if telemetry is disabled.

### 25.3 Telemetry acceptance

1. The user receives a first-run telemetry notice.
2. The user can opt out on first run or later.
3. Private or local-only sources do not emit remote telemetry.
4. Public install and update events can be aggregated for popularity and update metrics.

### 25.4 UX acceptance

1. A new user can understand the model in under five minutes.
2. Common operations require one command.
3. Debugging "why is this skill missing?" requires one command.
4. Every TUI action has a documented non-interactive CLI equivalent.

## 26. Test plan

### 26.1 Unit tests

- manifest parsing
- lockfile roundtrip
- spec validation
- overlay resolution
- conflict resolution
- root planning
- install candidate detection
- history ledger writes
- telemetry consent and suppression rules
- JSON output schemas

### 26.2 Integration tests

Matrix:

- OS: macOS / Linux / Windows
- mode: copy / symlink
- scope: workspace / user
- targets:
  - codex
  - gemini-cli
  - claude-code
  - github-copilot
  - amp
  - opencode
  - codex + gemini-cli + opencode
  - claude-code + github-copilot

### 26.3 Fixture repos

Create fixture repos for:

- canonical-only local skills in `.agents/skills/`
- imported skill with overlay
- same-name conflict
- nested monorepo skills
- broken symlink mode
- source repo with skills in `.opencode/skills/`
- source repo with skills in repo-root `skills/`
- modified installed skill blocking an update
- private-source install with telemetry suppression

## 27. Roadmap

### Phase 1 - core local manager

- `.agents/` workspace layout
- manifest and lockfile
- `install`, `update`, `sync`, `doctor`, `history`
- copy-based sync
- adapters for six targets
- local SQLite state store

### Phase 2 - overlays, rollback, and TUI

- overlay workflow
- pin, rollback, and detach
- terminal UI for history and inspection
- richer cleanup flows

### Phase 3 - telemetry and agent-native control plane

- remote telemetry pipeline
- MCP server
- JSON schema hardening
- agent-safe destructive-action model

### Phase 4 - ecosystem expansion

- more adapters
- package or export helpers
- optional registry integration
- richer security and audit features

## 28. Open questions

1. Should v1 support org or admin scope as read-only inputs, or defer entirely?
2. Should `skillctl pack` create a portable `.skill` archive in v1 or v2?
3. How much automatic repo-creation help should `update` offer when Git and GitHub CLI are present?
4. Should telemetry default behavior vary by environment, region, or enterprise policy?
5. Should user-scope neutral roots converge further if more agents standardize on `~/.agents/skills`?

## 29. Recommended default UX

For a new project:

```bash
skillctl init
skillctl sync
```

For installing a public skill:

```bash
skillctl install https://github.com/vercel/ai.git --name ai-sdk --scope workspace
skillctl doctor
```

For customizing and updating a public skill:

```bash
skillctl override ai-sdk
skillctl update ai-sdk
skillctl history ai-sdk
```

This simplicity is a product requirement, not a nice-to-have.

## Appendix A - compatibility matrix used by this spec

| Runtime | Workspace roots to support | User roots to support | Notable behavior |
|---|---|---|---|
| Codex | `.agents/skills` | `~/.agents/skills` | scans from CWD up to repo root; duplicate names may both appear; supports `agents/openai.yaml` |
| Claude Code | `.claude/skills` | `~/.claude/skills` | explicit precedence enterprise > personal > project; nested discovery; plugin skills supported |
| GitHub Copilot | `.github/skills` or `.claude/skills` | `~/.copilot/skills` or `~/.claude/skills` | project and personal skills; strong shared-root case with Claude |
| Gemini CLI | `.gemini/skills` or `.agents/skills` | `~/.gemini/skills` or `~/.agents/skills` | `.agents/skills` alias documented; precedence workspace > user > extension |
| Amp | `.agents/skills` | `~/.config/agents/skills` or `~/.config/amp/skills` | project skills in `.agents/skills`; user-wide precedence includes both neutral and Amp-specific roots |
| OpenCode | `.opencode/skills`, `.claude/skills`, or `.agents/skills` | `~/.config/opencode/skills`, `~/.claude/skills`, or `~/.agents/skills` | native `skill` tool; neutral and Claude-compatible roots both supported |

## Appendix B - research basis

This revision uses the following ecosystem facts as of 2026-03-19:

1. Agent Skills standardizes the skill directory structure and `SKILL.md` frontmatter, but not a universal lifecycle manager.  
   https://agentskills.io/specification

2. Codex reads from repo, user, admin, and system skill locations; scans `.agents/skills` from the current directory to repo root; supports `agents/openai.yaml`; and documents symlink support.  
   https://developers.openai.com/codex/skills

3. Claude Code uses `.claude/skills`, supports personal, project, and plugin scopes, defines precedence, and discovers nested `.claude/skills` directories.  
   https://code.claude.com/docs/en/skills

4. GitHub Copilot supports project skills in `.github/skills` or `.claude/skills`, and personal skills in `~/.copilot/skills` or `~/.claude/skills`.  
   https://docs.github.com/en/copilot/concepts/agents/about-agent-skills  
   https://docs.github.com/en/copilot/how-tos/use-copilot-agents/coding-agent/create-skills

5. Gemini CLI documents `.agents/skills` as an alias for workspace and user skill roots and provides first-class skill management commands.  
   https://geminicli.com/docs/cli/skills/

6. Amp documents project skills in `.agents/skills`, user-wide skills in `~/.config/agents/skills` and `~/.config/amp/skills`, and a built-in `amp skill` management flow.  
   https://ampcode.com/manual

7. OpenCode documents skills in `.opencode/skills`, `.claude/skills`, and `.agents/skills`, with corresponding global roots and a native `skill` tool.  
   https://opencode.ai/docs/skills

8. Real-world symlink issues remain visible in public issue trackers even where documentation describes symlink support, which justifies `copy` as the default.  
   https://github.com/openai/codex/issues/9898  
   https://github.com/openai/codex/issues/11314  
   https://github.com/openai/codex/issues/10470
