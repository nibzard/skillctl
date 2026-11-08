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
- `path`: canonical, cached, overlay, planned-root, and projected filesystem locations, including for a canonical local skill that was never installed
- `history`: installs, update checks, projections, overlays, rollbacks, forks, cleanup, and telemetry consent changes; a canonical local skill reports an empty ledger, because the ledger follows managed imports and `sync` records no projection event for a skill it projects straight from `.agents/skills`

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
- The skill is managed at user scope, so workspace scope plans no projection
  for it.
- The skill is a workspace canonical skill, so user scope plans no projection
  for it.

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
- `skillctl --scope user explain <skill>` and `skillctl --scope user sync`
  when the skill is managed at user scope, because a plain `skillctl sync`
  reads workspace scope and cannot project a user-scope import into it
- `skillctl --scope workspace explain <skill>` and
  `skillctl --scope workspace sync` when the skill is a workspace canonical
  skill, because `--scope user sync` cannot project it into user roots
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

## doctor Reports `manifest-missing`

Symptoms:

- the workspace holds skills under `.agents/skills` but no
  `.agents/skillctl.yaml`
- `sync` warns that it used the documented defaults
- `doctor` reports `manifest-missing`
- `verify`, `update`, or `restore` refuses with
  `no manifest at '.agents/skillctl.yaml'`
- `pin`, `rollback`, `override`, or `fork` refuses the same way on a skill the
  local state store covers, while a name nothing manages reports
  `is not installed`

Run:

```bash
skillctl init
```

What the code means:

- `manifest-missing`: no manifest file exists, so the diagnostics and the
  projections ran on the documented defaults instead of a manifest you chose
- `no manifest at ...`: a command that reads declared imports found none. It
  refuses with exit code `3` instead of inventing an empty declaration

The defaults are the ones `skillctl init` writes: the targets `codex`,
`gemini-cli`, and `opencode`, copy mode, and the default layout. The flow keeps
working, so the issue is a warning, not an error.

`sync`, `validate`, `explain`, `list`, `path`, and `doctor` run on those
defaults. `verify`, `update`, and `restore` answer from the imports and
overrides the manifest declares, so they refuse and name `skillctl init`
instead of reporting a workspace that declares nothing.

`pin`, `rollback`, `override`, and `fork` rewrite an import declaration, so they
need the manifest too. They resolve the skill name first: a name nothing
manages, such as a canonical local skill, hears `is not installed`, and a name
the local state store covers hears the `no manifest at ...` refusal that names
`skillctl init`.

Typical fixes:

- run `skillctl init` to create the manifest, then edit `targets:` when you
  need different runtimes
- commit `.agents/skillctl.yaml` so a teammate's checkout projects the same
  roots
- in a team checkout that lost its manifest, retrieve the committed file. A
  lockfile without a manifest pins imports nothing declares, and `verify`
  would call every pin stale

A directory with no skills, no lockfile, and no manifest never had a manifest
to lose. `doctor` stays quiet there, and the bundled user-scope skill keeps
working without one.

## Commands Refuse With `no lockfile at '.agents/skillctl.lock'`

Symptoms:

- `.agents/skillctl.yaml` exists but `.agents/skillctl.lock` does not
- `verify`, `update`, `pin`, `rollback`, `override`, or `fork` exits with code
  `3` and a refusal that names `.agents/skillctl.lock`
- `restore` exits with code `3` and says there is nothing to restore
- `doctor` reports a `lockfile-missing` warning
- `sync`, `validate`, `explain`, and `doctor` also report
  `enabled import '<name>' is missing from the lockfile` when the manifest
  declares imports

What the code means:

- `no lockfile at ...`: a command that reads or rewrites pins found no lockfile.
  It refuses instead of running on an empty set of pins
- `lockfile-missing`: `doctor` answered on an empty set of pins. The warning
  names both repairs, including the no-imports case where no per-import
  `missing-lockfile-entry` error can carry the loss

The refusal names two repairs, and they are not interchangeable:

- in a team checkout, retrieve the committed `.agents/skillctl.lock`
- run `skillctl init` only when this workspace pinned nothing yet

`skillctl init` writes an empty lockfile. An empty lockfile verifies zero
imports, so a workspace that lost a populated lockfile would look healthy with
no pins at all. Retrieve the committed file instead.

`sync`, `validate`, `explain`, and `doctor` read the manifest and the canonical
skills, so they keep answering. When the manifest declares imports, each of them
names the imports the missing lockfile leaves unpinned instead of working
silently. `doctor` reports the loss itself as a `lockfile-missing` warning as
long as the manifest exists, so a manifest that declares no imports still hears
about the missing file. `list`, `path`, and `history` keep answering.

## A Generated Copy Looks Stale

Symptoms:

- a runtime still shows older content
- a runtime still loads a skill that `skillctl` no longer manages
- `doctor` reports `projection-drift`
- `doctor` reports `wrong-precedence-root`
- `doctor` reports `missing-projection`
- `doctor` reports `blocked-projection`
- `doctor` reports `stale-generated-projection`

Run:

```bash
skillctl doctor
skillctl sync
skillctl explain <skill>
```

What the codes mean:

- `projection-drift`: a generated copy exists but does not match the active winner
- `missing-projection`: nothing is materialized at the planned root, so the
  runtime cannot load the skill there
- `blocked-projection`: the planned path is not a readable skill directory,
  usually a stray file or restrictive permissions
- `stale-generated-projection`: a generated copy remains although no candidate
  resolves to that skill any more, so every runtime assigned to the root keeps
  loading it. The same code covers a copy left in a documented root no enabled
  target plans any more, for example after the target set moved away from
  `.agents/skills`, and a copy left in a scope whose winner is managed in the
  other one, for example a workspace copy of a user-scope import

For `projection-drift`, read the reported fix. A copy that holds edits
`skillctl` did not record is preserved by `sync`, so the fix names the update
strategies that resolve those edits. A copy that only went stale against the
active winner is refreshed by `sync` itself.

Typical fixes:

- `skillctl sync` to refresh or materialize generated copies
- `skillctl sync` to prune a `stale-generated-projection` copy, or
  `skillctl clean` to remove every generated copy at once. When
  `projection.prune` is off, `sync` does not prune; the reported fix says so
- remove blocking entries or fix permissions on the planned root, then sync
- confirm the manifest still enables the target you care about
- use `skillctl path <skill>` to confirm the runtime root you are checking is the planned root

`skillctl explain <skill>` shows the observed state per target, including
`observed_presence`, `observed_matches_winner`, and the repair step. Its
summary line repeats the same repair, so an edited copy never gets a plain
`sync` recommendation that would preserve the copy, and a skill managed at
another scope never gets one that a sync in this scope cannot deliver. A
report that observes user scope names `skillctl --scope user sync` for the
same reason, because a plain `skillctl sync` reads workspace scope.

### `sync` reports a preserved copy

Symptoms:

- `sync` exits with `2` and prints `warning: preserved ... with unrecorded edits`
- the generated copy keeps your manual edits

Each generated copy records the content digest it was written with. A copy that
no longer matches that digest holds edits `skillctl` never made, so `sync`
keeps it instead of overwriting it. `install`, `clean`, and `remove` follow the
same rule.

Fixes:

- keep the edits and apply an update on top: `skillctl update <skill> --apply --strategy overlay`
- drop the edits after a durable backup: `skillctl update <skill> --apply --strategy discard`
- regenerate the copy without an update: `skillctl sync --discard`

Every advice line above names the `sync` command qualified with the scope the
report observes. A user-scope copy reads `skillctl --scope user sync
--discard`, because a plain `skillctl sync` reads workspace scope and leaves
the copy untouched.

For an unreviewed skill that contains `scripts/` and tracks a Git source, the
first two commands are refused by the trust gate while an upstream revision is
waiting, because both move the import to that revision. The repair advice from
`doctor`, `explain`, `sync`, and `clean` follows the same rule, and it reads the
local state to apply it: while the skill is pinned to a commit or a recorded
version, or while the newest recorded update check for the current pin reported
nothing to move to, the two update strategies are advised, because their repair
keeps the pinned revision. Run `skillctl update <skill>` to record such a check.

A script-free import is judged the same way, because the revision a repair would
move it to can add the `scripts/` directory the installed content never had. The
advice reads the newest recorded update check for the current pin: when that
check saw a waiting revision that adds `scripts/`, the two update strategies are
refused with exit code `4` and the advice names the commands below instead. A
check that saw a script-free waiting revision, or a script-free import no check
has observed, keeps the update strategies.

When the local state cannot clear the skill of a waiting revision, the advice
names commands that work in every state instead. Record a review with
`skillctl review <skill> --approve` to let the update strategies through again,
or use the commands the advice names for such a skill:

- keep the edits as local ownership: `skillctl update <skill> --apply --strategy fork`
- drop the edits after a durable backup: `skillctl sync --discard`

For a skill no install record holds, every `update` strategy is refused with
exit code `3` and `skill '<skill>' is not installed`. This covers a copy left
behind by `skillctl remove` and the generated copy of a canonical local skill in
`.agents/skills/`. `doctor`, `explain`, `sync`, and `clean` name only the repair
that works for such a copy:

- drop the edits after a durable backup: `skillctl sync --discard`
- keep the edits by moving them into a canonical skill under `.agents/skills/`,
  then run `skillctl sync`

The two update strategies also resolve a copy that sits in a root the current
plan no longer includes, and the leftover copy of a disabled import. Both
repairs read the digest the copy records, which is the same evidence `sync`
preserves the copy on. A copy written before digests were recorded is judged
against the content of the pinned source instead, so the same two strategies
resolve it. A repair keeps a disabled import disabled: the edits move
into its overlay, and the next `skillctl enable <skill>` projects them again.

Backups land under `~/.skillctl/store/discards/`, and the local ledger records
each discard.

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

A destination without a generated copy is not drift. `update` reports it under
`missing_projections` with the targets that expect it, keeps the recommendation
an intact workspace would get, and warns:

```text
warning: skill '<skill>' has no projection at '.agents/skills/<skill>' for codex, gemini-cli, opencode; run 'skillctl sync' to materialize the missing projection
```

The manifest plan decides which destinations the check expects, which is the
same plan `explain` and `doctor` read. A root the target set added before a
`sync` ran is therefore named as well, and a root the plan dropped is not,
because `sync` prunes that root instead of rebuilding it.

The run exits with `2` because of that warning. `skillctl sync` rebuilds the
copy, and an apply that moves the revision rebuilds it too. A run that leaves
the skill behind, such as `--strategy skip`, repeats the warning.

Applying an update:

```bash
skillctl update <skill> --apply
```

When edits block a clean apply, pick a strategy:

- `skillctl update <skill> --apply --strategy overlay` keeps the edits and applies the update
- `skillctl update <skill> --apply --strategy discard` backs the edits up, then applies
- `skillctl update <skill> --apply --strategy fork` takes local ownership instead
- `skillctl update <skill> --apply --strategy skip` keeps the current pin

The same strategies repair an edited copy of a folder or archive import. Such
imports track no upstream revision, so the apply resolves the edits and keeps
the pinned source content instead of moving to a new revision.

An import whose check reports up-to-date is repaired the same way. Nothing
upstream moves, so `overlay` keeps the edits on the pinned revision, `discard`
backs them up and regenerates the copy, and a plain `--apply` refuses with exit
code `3` because a clean strategy must not guess.

An overlay you edited without re-pinning it does not hide a copy edit. While the
overlay hashes to something other than its pin, the check judges every generated
copy against the content skillctl wrote for it, not against the effective
content. The copy edit stays visible, and the same strategies repair it. The run
warns about the overlay itself:

```text
warning: skill '<skill>' has a managed overlay that changed since the recorded
state; run 'skillctl override <skill>' to re-pin the change after reviewing it,
then commit the lockfile together with the overlay
```

A repair also re-pins the overlay, because the apply recomputes the overlay
digest from the directory it leaves behind. Re-pin with `skillctl override
<skill>` first when you want to review the overlay change before any copy
repair touches it.

An import pinned to a commit reports outcome `commit-pinned` instead. A commit
names one immutable revision, so the check cannot observe a newer one and
reports `latest_revision: null` with a note that says so. That is a named
state, not a failure, and the same repair strategies still resolve unmanaged
edits without moving the pinned revision. To track upstream again, pin a
movable reference:

```bash
skillctl pin <skill> main
```

A requested apply that skips every skill exits with `2` and prints one warning
per skip. Read the reasons: a skill whose source kind cannot be applied, such as
a folder import without edits, is reported there instead of failing silently.

Typical fixes:

- `skillctl override <skill>` if you want a small managed customization layer
- `skillctl fork <skill>` if you want full local ownership in `.agents/skills/<skill>/`
- `skillctl pin <skill> <ref>` if you want to stay on a known revision deliberately

### `update --apply` reports a stale or tampered plan

Symptoms:

- `plan ... is stale: ... but the plan was built against ...`
- `plan ... is tampered: ... digests to ... but claims ...`

Both exit with `3` and change nothing. A stale plan was written against local
state that has since moved. A tampered plan document was edited by hand.

Fix: rebuild the plan and apply the fresh one.

```bash
skillctl update --plan update-plan.json
skillctl update --apply --plan-file update-plan.json
```

### A plan document applies nothing

Symptom:

```text
warning: the plan document contains no plans, so the requested apply changed nothing; rebuild it with 'skillctl update --plan'
Applied 0 updates with the clean strategy, skipped 0.
```

The command exits with `2`. The document held no plans, or none that named a
managed skill, so the apply had nothing to do.

A skill whose edits block an update still contributes a plan, and writing that
plan warns which strategy resolves the edits. Pick that strategy, rebuild the
document, and apply it:

```bash
skillctl update <skill> --plan update-plan.json --strategy overlay
skillctl update --apply --plan-file update-plan.json
```

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

Imported skills start as `imported-unreviewed`. If the effective skill contains a top-level `scripts/` directory, `doctor` and `update` surface elevated warnings. The gate also fires when a waiting upstream revision introduces such a directory, so executable content cannot arrive without a review.

Look for:

- `script-risk`
- trust warnings in `doctor`
- trust warnings in `update --json`

The gate is enforced, not advisory. While an update is available for such a
skill, `update --apply` refuses before changing anything:

```text
trust gate is blocking update apply for '<skill>' until it is reviewed or forked; record a local review with 'skillctl review <skill> --approve', or take local ownership with 'skillctl update <skill> --apply --strategy fork'
```

When the installed content is script-free and the incoming revision adds the
scripts, the refusal names that revision instead:

```text
trust gate is blocking update apply for '<skill>' until it is reviewed or forked; the incoming revision <revision> adds a top-level scripts/ directory no recorded review covers; record a local review with 'skillctl review <skill> --approve', or take local ownership with 'skillctl update <skill> --apply --strategy fork'
```

The command exits with `4` and keeps the pinned revision, the stored import, and
every generated copy unchanged. `--strategy overlay` and `--strategy discard`
are refused the same way, because they also move the import to the new upstream
revision. That still holds when a generated copy holds unmanaged edits: the
check reports outcome `blocked` for the edits, and a repair apply refuses with
`4` while leaving the edits where they are. A plan document is refused too,
because the apply engine re-derives the gate from the revision it checks out.

`pin` and `rollback` install the tree an update apply installs, so the same
gate decides them and the same exit code refuses them:

```text
trust gate is blocking pin for '<skill>' until it is reviewed or forked; record a local review with 'skillctl review <skill> --approve', or take local ownership with 'skillctl fork <skill>'
```

A rollback prints the same refusal with `rollback` in place of `pin`. A gated
rollback keeps its recovery path: the review it names covers the content the
workspace holds now, and the same rollback then succeeds, so one review is the
way back to a previous revision.

Typical responses:

- read the imported content and the incoming revision, then record the review: `skillctl review <skill> --approve`
- keep the current pin until the skill is understood
- `skillctl fork <skill>` or `skillctl update <skill> --apply --strategy fork` to take full local ownership

### Recording a review

`skillctl review <skill>` reports the state on record and the content it covers:

```bash
skillctl review ai-sdk
skillctl review ai-sdk --approve --note 'scripts inspected, safe'
```

`--approve` records that a person read the effective content. The import moves
to `imported-reviewed`, `doctor` stops reporting `script-risk` for it, and the
apply, pin, or rollback the gate held back succeeds. `--revoke` withdraws the
decision.

A review covers one effective version. When an update, a pin, or a rollback
moves the import to new content, that content is unreviewed again, the warning
and the gate return, and a fresh review is needed. The decision is local state
in `~/.skillctl/state.db` with its own `trust-review` history entry; it is not
written to `.agents/skillctl.lock` and does not travel with the repository.

## Lockfile Or Stored Import Drift

Look for:

- `stale-lockfile-entry`
- `missing-lockfile-entry`
- `missing-import-store`
- `missing-import-source`
- `import-store-divergence`

Meaning:

- the manifest and lockfile no longer agree
- cached imported content is gone
- a managed import exists in one layer but not another
- stored imported content no longer hashes to its pin

A disabled import is not drift. `skillctl disable <skill>` keeps the manifest
declaration and the lockfile pin on purpose, so a later `enable` or
`restore --frozen` still has the pinned identity. `explain`, `inspect`, and
`doctor` report that state as `disabled-import` and never as
`stale-lockfile-entry`.

Typical fixes:

- a disabled import: `skillctl enable <skill>`, or `skillctl remove <skill>`
  to drop the import and its lockfile entry
- `stale-lockfile-entry`: run `skillctl remove <skill>` to clear the entry, or
  reinstall it with `skillctl install <source> --name <skill>` when you want
  the import back
- `missing-import-store`: `skillctl restore --frozen`
- rerun `skillctl sync` after cleanup

Do not edit `.agents/skillctl.lock` by hand. A lockfile entry the manifest no
longer declares is stale, and `skillctl remove <skill>` clears it directly. An
explicit `--scope user` selector refuses such an entry, because the lockfile
pins imports by name without a scope, so run the removal without a selector or
with `--scope workspace`. Hand-edited entries break `restore --frozen`, which
verifies the pinned digests instead of trusting the file.

### A symlink projection wrote the import store

Symptoms:

- `verify` exits with `3` and reports `import '<skill>' does not match the pinned content`
- `update <skill>` exits with `3` with `stored import '<skill>' content hash mismatch`
- `doctor` and `explain <skill>` report an `import-store-divergence` issue
- `sync` exits with `2`, keeps the projection, and reports a preserved copy with
  the reason `diverged-import-store`

You use `projection.mode: symlink`. A projected file of an import is a symlink
into `~/.skillctl/store/imports`, so an edit that follows the link rewrote the
pinned bytes. The projection's links are unchanged, which is why the copy itself
still looks clean and the mismatch appears in the store.

Fixes:

```bash
skillctl restore --frozen    # rebuild the pinned content from the retained artifact
skillctl install <source> --name <skill> --scope workspace    # re-pin the changed content
```

Both repairs rewrite the store, so both drop the edit that caused the mismatch.
Run `skillctl fork <skill>` first when you want to keep it: the fork copies the
stored bytes into a canonical skill you own.

Do not run `skillctl update <skill>` for this. The check resolves through the
same diverged store, so it exits with `3` and repeats the mismatch. That holds
for every strategy, `--strategy fork` included, because an update apply runs
its check through the store before any strategy acts. The fork that keeps the
edit is the standalone `skillctl fork <skill>` command, which reads the stored
bytes directly.
`skillctl sync --discard` does not repair it either, because the copy holds
links, not the edit. Prefer `projection.mode: copy` when you edit projected
files directly; copy mode keeps such an edit in the copy, where `sync`
preserves it and the update strategies resolve it.

The divergence blocks only its own import. A whole-workspace `skillctl update`
(or one that names another skill) still runs: the diverged import reports a
`blocked` plan with the same two repairs in its notes, an `--apply` run skips
it with that reason, and the other skills update as usual. The apply's final
`sync` still preserves the diverged copy.

## Restore Fails Or Verify Reports A Problem

`skillctl restore --frozen` reconstructs every enabled import from
`.agents/skillctl.lock`. It fails rather than guessing. Each failure names the
import and the reason.

**A fresh clone reports a missing import cache.**

That is expected. The import cache is local to each checkout. Run:

```bash
skillctl restore --frozen
```

**Restore or verify refuses with `no manifest at '.agents/skillctl.yaml'`.**

The checkout holds no manifest, so no imports or overrides are declared. These
commands do not run on the documented defaults, because defaults would invent
an empty declaration and call every pinned import stale. In a team checkout,
retrieve the committed `.agents/skillctl.yaml`; otherwise run:

```bash
skillctl init
```

**Restore or verify refuses with `no lockfile at '.agents/skillctl.lock'`.**

The checkout holds a manifest but no lockfile, so nothing is pinned. In a team
checkout, retrieve the committed `.agents/skillctl.lock`; `skillctl init`
writes an empty lockfile, so run it only for a workspace that pinned nothing
yet. See [Commands Refuse With `no lockfile at '.agents/skillctl.lock'`](#commands-refuse-with-no-lockfile-at-agentsskillctllock).

**Restore fails with `offline restore requires a verified cached artifact`.**

You used `--offline`, and neither the import cache nor the retained artifacts
under `~/.skillctl/store/artifacts/` hold the content. Either rerun without
`--offline`, or restore the artifact cache from a machine that has it.

**Restore fails with `locked revision ... is unavailable from <url>`.**

The source was contacted but does not serve the pinned commit. Usually the
history was rewritten or force-pushed, so the commit no longer exists upstream.
A hand-edited `revision.resolved` in `.agents/skillctl.lock` fails the same way,
so compare the pinned value with the commit the source actually serves. This is
not a network problem: a fetch that cannot reach the source reports `git fetch
failed` instead. `skillctl` never substitutes the current branch HEAD for a
locked revision. Choose a new revision explicitly with `skillctl update <skill>`
and `skillctl pin <skill> <ref>`, or restore from a machine that still retains
the artifact.

**Restore fails with `content at the locked revision digests to ... but the
lockfile pins ...`.**

The source history was rewritten so the same commit name now holds different
content, or the lockfile was edited by hand. Do not accept the new content
silently. Review it, then re-pin it deliberately.

**Restore fails with `locked local source ... is not available on this machine`.**

The import came from a local folder or archive, and that path does not exist
here. Folder and archive imports have no upstream to fall back on. Either fetch
the recorded path, copy the artifact cache from the machine that produced the
lockfile, or replace the import with a portable source.

**A fresh clone needs to replace an import it cannot restore.**

`install` refuses to replace an import the manifest already declares, and that
refusal names the repair. `remove` resolves an install through the local state
store first, then falls back to the import the committed manifest and lockfile
declare, so this works on a fresh clone without editing either file by hand:

```bash
skillctl remove <skill>
skillctl install <portable-source> --name <skill>
```

The remove reports a warning that names the missing install record. When
another enabled import is still unrestored, the projection rebuild is skipped
with a warning instead of refusing the removal. Run `skillctl sync` after the
remaining imports are restored or removed.

**A fresh clone wants to keep the pin but stop projecting an unrestored
import.**

`enable` and `disable` resolve the import the same way `remove` does. Both act
on the import the committed manifest and lockfile declare when no install
record exists yet:

```bash
skillctl disable <skill>
```

The disable keeps the lockfile pin, prunes the generated copies of the import,
and leaves `skillctl sync` usable again. `skillctl enable <skill>` sets the pin
back to enabled. The projection rebuild is then skipped with a warning until the
source is restored or the import is replaced.

**`explain skillctl` or `inspect skillctl` reports the skill is not visible in
workspace scope.**

That is the expected answer for an unscoped request. `skillctl` installs the
bundled skill at user scope only, so the workspace graph holds no candidate for
that name. The report names the scope that owns the name and the command that
reads it:

```bash
skillctl --scope user explain skillctl
```

A checkout that also declares a workspace import named `skillctl` resolves that
import instead, and a checkout whose lockfile still pins the name keeps the
`stale-lockfile-entry` report.

**`disable skillctl` fails instead of disabling the bundled skill.**

The bundled user-scope skill is locally owned. It has no manifest import to
toggle, so `skillctl disable skillctl` and `skillctl --scope user disable
skillctl` refuse it with exit code `3` and name the supported exit:

```bash
skillctl --scope user remove skillctl
```

A checkout that also declares a workspace import named `skillctl` reports the
shared scope-resolution error for an unscoped request first. Use
`--scope workspace` to reach that import.

**`override skillctl` or `fork skillctl` fails instead of acting on the bundled
skill.**

The bundled user-scope skill is locally owned, so it carries no managed import
for an overlay or a fork to act on. `skillctl override skillctl`, `skillctl fork
skillctl`, and their `--scope user` forms refuse it with exit code `3` and name
the supported exits: stop projecting it with `skillctl --scope user remove
skillctl`, or manage a copy of the content with `skillctl install <source>
--name skillctl --scope workspace` and run override or fork on that import.

A checkout that also declares a workspace import named `skillctl` keeps the
shared scope-resolution error for an unscoped request, extended with the
`--scope workspace` command that reaches the declared import. Older versions
instead paired the bundled install record with that import's lockfile entry and
reported a stale plan no command could repair.

**`override <skill>` or `fork <skill>` fails with `has no stored source in ...`.**

The checkout declares the import and the lockfile pins it, but the import cache
and the state store are local to each machine, so a fresh clone holds neither
yet. Run the restore the refusal names, then run the command again:

```bash
skillctl restore --frozen
skillctl override <skill>
```

A source this checkout cannot reconstruct again has no restore path, so clear
the import with `skillctl remove <skill>` and install a portable replacement. A
refusal that names a missing install record while the stored source exists is
the restore repair again: `restore --frozen` verifies the cached content against
the pinned digests and records the install again, so it contacts no source.

**Restore fails with `the lockfile pins overlay content ... but the overlay
directory ... is absent from this checkout`.**

The lockfile pins a digest for an overlay, and the checkout does not carry the
overlay files. The lockfile records what the overlay must hash to, but it does
not carry the files themselves. Commit `.agents/overlays/<skill>/` next to
`.agents/skillctl.yaml` and `.agents/skillctl.lock`, then restore again. The
failure names the missing directory and the pinned digest, and `skillctl
verify` reports the same state as `portability.restorable: false`.

When the overlay directory exists but the restore still refuses it, run
`skillctl verify`. It reports the overlay as `mismatch`, names the observed
digest and the pinned digest, and warns that an overlay has to be re-pinned
deliberately with `skillctl override <skill>` after you review the change.

**`sync` exits with code 2 and warns that it projects overlay content the
lockfile does not pin.**

You edited an overlay file, so the overlay directory no longer hashes to the
digest `.agents/skillctl.lock` pins. `sync` still projects the edit, because
that is the content you asked for, but committing that pair gives every
teammate a `restore --frozen` that fails on the digest mismatch. Review the
edit, re-pin it, and commit the lockfile with the overlay:

```bash
skillctl override <skill>
```

`override` never overwrites existing overlay files, so this only records the
new digest. The warning clears on the next `sync`, and `skillctl verify`
reports the same state while the pin is stale.

**`verify` exits with code 3 but the cache looks fine.**

`verify` separates integrity from everything else. Exit code 3 means either the
cached content does not match the pinned digests, or an import cannot be
reconstructed on this machine. That second case includes a lockfile that pins
overlay content the checkout does not carry. A missing cache that can be
restored is exit code 2 with a warning, not a failure.

**Two teammates get different lockfiles for the same skill.**

Local timestamps and machine paths are not part of dependency identity and are
not recorded in the lockfile. If the lockfiles still differ, compare the
`revision.resolved` and `bundle.digest` fields; those are the identity.

## Local State Recovery, Locks, And Migrations

Run:

```bash
skillctl doctor --repair-state
skillctl doctor --repair-state --apply
```

What the output means:

- `pending_operations`: a lifecycle operation was interrupted before it
  finished. Its journal can restore the workspace and database to the state
  before the operation started.
- `legacy_tables`: schema-v1 tables still exist beside the current schema.
  `copy-and-drop` and `drop-only` rows can be recovered automatically.
  `ambiguous` rows cannot be attributed to one workspace, so they stay
  untouched and are listed under `unresolved`.
- `state_database_unreadable`: SQLite cannot read `~/.skillctl/state.db` at all,
  for example because another program truncated it or wrote a different file
  format over it. The repair moves the corrupt file aside and rebuilds an empty
  database. See the next section.
- `migration_backups`: database copies kept before a schema migration. The
  three newest are retained.

Typical fixes:

- preview first with `skillctl doctor --repair-state`; it changes nothing
- apply with `skillctl doctor --repair-state --apply` to restore interrupted
  operations and recover unambiguous rows
- keep the database and its backups if a repair reports `ambiguous` rows; the
  report names the tables and the reason, so resolve ownership by hand instead
  of deleting data
- run the next ordinary command after a crash; it also restores interrupted
  operations before it starts

How the guarantees work:

- every schema migration runs in one SQLite transaction, so an interrupted
  migration leaves the previous schema and rows instead of a database that
  looks current but holds no data
- one process at a time mutates the shared state in `~/.skillctl`; a second
  process waits up to 15 seconds and then fails with the holder's process id
- set `SKILLCTL_LOCK_TIMEOUT_MS` to change that wait, for example in scripts
  that prefer to fail fast
- read commands such as `list` and `history` do not take that lock, so they can
  run while another process commits; they wait up to 60 seconds for that commit
  instead of failing with `database is locked`, and
  `SKILLCTL_STATE_BUSY_TIMEOUT_MS` changes that wait
- a read that still runs out of that wait fails with `timed out after ... ms
  waiting to ... in local state store ...: database is locked`. The message
  names `SKILLCTL_STATE_BUSY_TIMEOUT_MS`, because that is the wait it exhausted.
  It also names `SKILLCTL_LOCK_TIMEOUT_MS`, which controls the other wait and
  does not help here
- a database is restored through SQLite, never by copying a file over a live
  database

## Every State-Backed Command Fails With `not a readable SQLite database`

A corrupt `~/.skillctl/state.db` blocks every command that reads local state:
`list`, `history`, `sync`, and `restore`. The failure names the recovery:

```text
local state store '<home>/.skillctl/state.db' is not a readable SQLite database:
file is not a database. Move the corrupt file aside or run
'skillctl doctor --repair-state --apply'. The next command rebuilds local
state from .agents/skillctl.yaml and .agents/skillctl.lock
```

Plain `skillctl doctor` is the exception. It keeps answering with a partial
report, exit code 3, and one `state-database-unreadable` error issue:

```text
error: the state database is not a readable SQLite database: file is not a
database, so this report skips the checks that read local state; fix: run
'skillctl doctor --repair-state --apply' to move the corrupt file aside, or
move it aside yourself; the next command rebuilds local state from
.agents/skillctl.yaml and .agents/skillctl.lock
```

The report is partial on purpose. The checks that read local state, such as
trust labels, projection records, and the bundled user-scope skill, wait until
the database is repaired. Everything that reads only the workspace, such as
skill validation, shadowing, and conflicts, still runs.

`skillctl verify` keeps working, because it reads the manifest, the lockfile,
and the import cache instead of local state. That is your proof that the
dependency state of the workspace is intact.

Recover with the repair entry point:

```bash
skillctl doctor --repair-state          # reports state_database_unreadable
skillctl doctor --repair-state --apply  # moves the corrupt file aside
skillctl restore --frozen               # rebuilds the import records
skillctl sync                           # rebuilds the projections
```

Or move the file aside yourself. The result is the same:

```bash
mv ~/.skillctl/state.db ~/.skillctl/state.db.corrupt
skillctl list
```

What the repair does:

- nothing is deleted. The corrupt file is renamed to
  `~/.skillctl/state.db.corrupt.<timestamp>` and its `-wal`, `-shm`, and
  `-journal` sidecars move with it, because SQLite refuses a fresh database
  that still carries a stale sidecar
- a new, empty `state.db` with the current schema is created. The repair
  creates it during `--apply`; after a manual move-aside, the next command
  creates it
- `restore --frozen` rebuilds the managed-import records from
  `.agents/skillctl.yaml` and `.agents/skillctl.lock`, and `sync` rebuilds the
  runtime projections from them
- if a lifecycle operation was interrupted at the same time, the repair moves
  the corrupt file aside first and then restores that operation, so the
  database snapshot it recorded is recovered instead of lost

What you lose:

- history entries, review decisions, pins, and update checks recorded only in
  the old database. The lockfile does not carry them, so it cannot restore
  them
- record review decisions again with `skillctl review <skill> --approve` for
  imports that hold scripts, or the trust gate blocks the next
  `update --apply`
- check `skillctl telemetry status` if you had telemetry disabled; consent
  also lived in the old database
- keep the quarantined file until you are sure you no longer need it

## Telemetry Questions

Check current state:

```bash
skillctl telemetry status
```

Disable telemetry:

```bash
skillctl telemetry disable
```

Remember:

- local history stays on
- install and update events are classified locally, and nothing is transmitted
  today because no emission backend exists
- private repositories, local paths, skill contents, and arbitrary diffs never
  leave your machine

## `tui` Looks Empty Or Shows Unexpected State

`skillctl tui` is implemented. It opens a read-only dashboard over the same install, update, explain, and history state used by the CLI, and opening it does not bootstrap bundled skills or write new history entries.

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
- if the update line says `up-to-date` right after `skillctl update <skill> --apply`, that is the dashboard fitting the recorded check to the installed revision
- if the update line says `update-available` right after `skillctl rollback <skill>` or `skillctl pin <skill> <older-revision>`, that is the same fit: the recorded check named the newer revision as the newest one it saw, and the install no longer holds it. Run `skillctl update <skill>` to record a fresh check
- `skillctl --json list` reports the same fitted view in `skills[].latest_update.outcome`. The check row in `skillctl history <skill>` keeps the observation it made before the apply or the rollback
- if you need machine-readable dashboard state, use `--json`; it returns the normal response envelope with aggregated skill cards and history data

## MCP Client Cannot Use `mcp serve`

`skillctl mcp serve` is implemented. It starts a stdio MCP bridge that exposes the v1 lifecycle tools and returns the same JSON response envelope the CLI emits with `--json`.

What to check:

- send `initialize` before `tools/list` or `tools/call`, then send `notifications/initialized`
- use newline-delimited JSON-RPC over stdin/stdout
- use a supported protocol version: `2025-03-26`, `2025-06-18`, or `2025-11-25`
- call one of the shipped v1 tools: `skills_list`, `skills_discover`, `skills_install`, `skills_remove`, `skills_sync`, `skills_restore`, `skills_verify`, `skills_update`, `skills_update_apply`, `skills_rollback`, `skills_history`, `skills_explain`, `skills_override_create`, `skills_validate`, `skills_doctor`, or `skills_telemetry_status`
- pass tool arguments as a JSON object; required fields are enforced, so `skills_remove` needs `{"skill":"<name>"}`, `skills_discover` needs `{"url":"<origin-or-index-url>"}`, `skills_rollback` needs both `skill` and `version_or_commit`, and `skills_restore` needs an explicit `frozen` boolean because `frozen=false` may rewrite `.agents/skillctl.lock`

Minimal handshake:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"example-client","version":"1.0.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
```

If you do not need MCP specifically, the simplest automation path is still the plain CLI with `--json`.
