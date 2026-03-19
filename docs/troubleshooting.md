# Troubleshooting skillctl

Use this guide when a skill is missing, stale, shadowed, detached, or otherwise not behaving the way you expect.

## Start Here

These four commands cover most failures:

```bash
skillctl doctor
skillctl explain <skill>
skillctl path <skill>
skillctl history <skill>
```

What each one tells you:

- `doctor`: workspace-wide validation, projection drift, precedence, trust, and runtime-facing problems
- `explain`: the winner for one projected skill name, shadowed candidates, target visibility, and drift
- `path`: canonical, cached, overlay, planned-root, and projected filesystem locations
- `history`: installs, update checks, projections, overlays, rollbacks, forks, cleanup, and telemetry consent changes

## A Skill Is Missing In One Runtime

Run:

```bash
skillctl explain <skill> --target <runtime>
skillctl doctor --target <runtime>
skillctl path <skill> --target <runtime>
```

Common causes:

- The import is disabled.
- The manifest target is not enabled.
- The planned runtime root changed and projections are stale.
- Another skill with the same projected name won.
- The stored import or overlay root is missing.

Look for these issue codes:

- `disabled-import`
- `missing-import-store`
- `missing-import-source`
- `wrong-precedence-root`
- `projection-drift`
- `shadowed-skill`
- `projected-name-conflict`

Typical fixes:

- `skillctl enable <skill>`
- `skillctl sync`
- remove or rename one conflicting source
- reinstall the skill if the stored import is gone

## doctor Reports `shadowed-skill` Or `projected-name-conflict`

Meaning:

- `shadowed-skill`: one candidate won and another candidate with the same projected `name` lost
- `projected-name-conflict`: resolution could not pick a single winner

Run:

```bash
skillctl explain <skill>
```

What to inspect:

- `winner`: the active candidate
- `shadowed`: losing candidates
- `why`: whether manifest priority or source-class precedence chose the winner

Typical fixes:

- remove one of the competing sources
- disable one managed import
- adjust manifest priorities when the project adds that configuration

## A Generated Copy Looks Stale

Symptoms:

- a runtime still shows older content
- `doctor` reports `projection-drift`
- `doctor` reports `wrong-precedence-root`

Run:

```bash
skillctl doctor
skillctl sync
skillctl explain <skill>
```

Typical fixes:

- `skillctl sync` to refresh generated copies
- confirm the manifest still enables the target you care about
- use `skillctl path <skill>` to confirm the runtime root you are checking is the planned root

## update Reports Drift, Overlays, Or Detached State

Run:

```bash
skillctl update <skill>
skillctl explain <skill>
skillctl history <skill>
```

What the update planner is telling you:

- `apply`: the imported skill looks safe to refresh
- `create-overlay`: local changes should move into `.agents/overlays/<skill>/`
- `detach`: you now own the skill locally and upstream lifecycle management should stop
- `skip`: keep the current pin for now

Important state flags:

- `overlay_detected`: the skill already has a managed overlay
- `local_modification_detected`: generated or cached content drifted outside the managed overlay flow
- `detached` or `forked`: the skill no longer tracks upstream updates

Typical fixes:

- `skillctl override <skill>` if you want a small managed customization layer
- `skillctl fork <skill>` if you want full local ownership in `.agents/skills/<skill>/`
- `skillctl pin <skill> <ref>` if you want to stay on a known revision deliberately

## doctor Reports Overlay Problems

Look for these issue codes:

- `missing-overlay-root`
- `invalid-overlay-path`
- `invalid-overlay-mapping`

Meaning:

- the overlay directory does not exist
- an overlay path is not a portable relative path
- an overlay file does not match a file in the imported skill

Run:

```bash
skillctl explain <skill>
skillctl path <skill>
```

Typical fixes:

- rerun `skillctl override <skill>` to recreate the overlay root
- remove invalid paths such as `.` or `..`
- delete unmatched overlay files or add the target file upstream before overriding it

## Install Fails In Non-Interactive Mode

Typical error:

```text
interactive input is required for command 'install'
```

Meaning:

- the source exposed multiple candidates
- the source or scope selection was ambiguous
- you disabled prompts with `--no-input`

Use one of these:

```bash
skillctl install <source> --interactive
skillctl install <source> --name <skill> --scope workspace
skillctl install <source> --name <skill> --scope user
```

Non-interactive install never guesses. If the exact name is ambiguous, narrow the source or resolve the duplicate names.

## Trust Or Script Risk Warnings

Imported skills start as `imported-unreviewed`. If the effective skill contains a top-level `scripts/` directory, `doctor` and `update` surface elevated warnings.

Look for:

- `script-risk`
- trust warnings in `doctor`
- trust warnings in `update --json`

Typical responses:

- review the imported skill contents
- keep the current pin until the skill is understood
- `skillctl fork <skill>` if you want to take full local ownership

## Lockfile Or Stored Import Drift

Look for:

- `stale-lockfile-entry`
- `missing-lockfile-entry`
- `missing-import-store`
- `missing-import-source`

Meaning:

- the manifest and lockfile no longer agree
- cached imported content is gone
- a managed import exists in one layer but not another

Typical fixes:

- reinstall the skill
- remove stale manifest or lockfile entries
- rerun `skillctl sync` after cleanup

## Telemetry Questions

Check current state:

```bash
skillctl telemetry status
```

Disable remote telemetry:

```bash
skillctl telemetry disable
```

Remember:

- local history stays on
- only public-source install and update events are eligible for remote emission
- private repositories, local paths, skill contents, and arbitrary diffs are never sent remotely

## `tui` Looks Empty Or Shows Unexpected State

`skillctl tui` is implemented. It opens a read-only dashboard over the same install, update, explain, and history state used by the CLI, and opening it does not write new history entries.

Run:

```bash
skillctl tui
skillctl --scope workspace --name <skill> tui
skillctl --target <runtime> tui
skillctl --json --name <skill> tui
```

What to check:

- if a skill card is missing, remove an overly narrow `--name`, `--scope`, or `--target` filter
- if the dashboard shows stale visibility or drift, follow the mapped CLI actions: `skillctl update [skill]`, `skillctl explain <skill>`, `skillctl path <skill>`, `skillctl history [skill]`, `skillctl pin <skill> <ref>`, or `skillctl rollback <skill> <version-or-commit>`
- if you need machine-readable dashboard state, use `--json`; it returns the normal response envelope with aggregated skill cards and history data

## MCP Client Cannot Use `mcp serve`

`skillctl mcp serve` is implemented. It starts a stdio MCP bridge that exposes the v1 lifecycle tools and returns the same JSON response envelope the CLI emits with `--json`.

What to check:

- send `initialize` before `tools/list` or `tools/call`, then send `notifications/initialized`
- use newline-delimited JSON-RPC over stdin/stdout
- use a supported protocol version: `2025-03-26`, `2025-06-18`, or `2025-11-25`
- call one of the shipped v1 tools: `skills_list`, `skills_install`, `skills_remove`, `skills_sync`, `skills_update`, `skills_rollback`, `skills_history`, `skills_explain`, `skills_override_create`, `skills_validate`, `skills_doctor`, or `skills_telemetry_status`
- pass tool arguments as a JSON object; required fields are enforced, so `skills_remove` needs `{"skill":"<name>"}` and `skills_rollback` needs both `skill` and `version_or_commit`

Minimal handshake:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"example-client","version":"1.0.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
```

If you do not need MCP specifically, the simplest automation path is still the plain CLI with `--json`.
