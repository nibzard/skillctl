# skillctl User Guide

`skillctl` is a local-first manager for the open `SKILL.md` ecosystem. It gives you one place to install, inspect, customize, and maintain skills across the agent runtimes you actually use.

This guide starts with a quickstart, then expands into the longer day-to-day workflows:

- creating a workspace
- installing skills from Git, local directories, archives, or well-known discovery indexes
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

If a teammate already committed `.agents/skillctl.lock`, reconstruct their
exact pinned skills instead of installing again:

```bash
skillctl restore --frozen
skillctl verify
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
- `~/.skillctl/store/imports/`: cached imported skill subtrees, namespaced by scope and workspace
- `~/.skillctl/store/artifacts/`: immutable retained bundles, addressed by canonical bundle digest
- `~/.skillctl/store/discards/`: durable backups taken before an explicit discard

The repository files answer different questions:

- `.agents/skillctl.yaml`: what the workspace intends to use
- `.agents/skillctl.lock`: the exact imported revisions, digests, and bundle identities currently pinned
- `.agents/overlays/`: the overlay files behind the overlay digests the lockfile pins
- `~/.skillctl/state.db`: what `skillctl` has observed and recorded locally over time

The first three are the committable portable state. Commit them together: a
lockfile that pins an overlay digest is only reproducible in a checkout that
also carries the overlay files.

Both directories under `~/.skillctl/store/` are caches. They can be deleted at
any time. Deleting them changes how a later `restore` reconstructs content, and
never changes what content is acceptable.

## Scopes, Targets, And Projection Roots

`skillctl` can manage skills in two scopes:

- `workspace`: the skill belongs to the current repository
- `user`: the skill is installed into your home directory for general use

Workspace scope is the default for project-specific dependencies. User scope is useful for always-on utility skills.

Commands that read state are scoped too. Without `--scope`, `explain`,
`inspect`, `doctor`, and `sync` read workspace scope, and `--scope user` reads
the user one. A skill installed at user scope still resolves as the winner of
an unscoped `explain`, but workspace scope plans no projection for it, so the
report says so instead of advising a `sync` that cannot change it. The bundled
`skillctl` skill is installed at user scope only, so an unscoped `explain` or
`inspect` reports that absence from workspace scope and points at
`skillctl --scope user explain skillctl`. The reverse holds too. A workspace
canonical skill still resolves as the winner of
`skillctl --scope user explain <skill>`, but user scope plans no projection for
it, so that report says so and points at
`skillctl --scope workspace explain <skill>`.

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
- a well-known discovery index or origin URL

Examples:

```bash
skillctl install https://github.com/acme/skills.git --name ai-sdk --scope workspace
skillctl install ../shared-skills --interactive
skillctl install ./skills.tar.gz --name release-notes --scope user
skillctl install https://skills.example.com --name code-review --scope workspace
```

A well-known source installs one entry of the discovery index a site publishes
at `/.well-known/agent-skills/index.json`. Pass an origin URL
(`https://skills.example.com`) or an index URL
(`https://cdn.example.com/skills/index.json`), and select the entry with
`--name`. The artifact is downloaded, verified against the digest the index
declares, and recorded as a `well-known` import whose pinned revision is that
verified digest. Use `skillctl discover <url>` first to see what an index
declares.

An origin-trust policy gates every discovery read. A loopback origin serves a
local index. An HTTPS origin that is not a verified publisher installs with a
warning, and its telemetry events stay suppressed. A plain-HTTP origin off
loopback is refused, because an attacker on the network could serve an index
whose artifacts match the digests the attacker declared.

`skillctl discover <url>` prints the index report without installing. The
report is read-only: it writes no workspace file and touches no local state.
Over MCP, `skills_discover` returns the same JSON envelope as
`skillctl discover <url> --json`.

### The discovery boundary

Local skill management and remote discovery stay separate.

- `skillctl` manages canonical skills in `.agents/skills/`, overlays in
  `.agents/overlays/`, and pins in `.agents/skillctl.lock`. That layout is the
  storage model, and it stays local.
- A discovery index is an interoperability layer. `skillctl` reads an index,
  verifies the digest each entry declares, and turns one entry into a normal
  managed import. An index is never a projection source and never a record
  `skillctl` writes.

Publishing support was evaluated on 2026-11-08 and stays out. The draft RFC
carries its 0.2.0 schema on a branch while its `main` branch still documents
the incompatible 0.1.0 format, and this project has verified exactly one live
publisher. Generating an `index.json` would couple a draft format to the
lifecycle before either side is stable. Revisit when the RFC merges a stable
schema version and several public publishers verify against it. A later
publishing task must stay separate from runtime projection, the way this
section keeps it separate from the local storage model.

Skills are detected in the same folders for directories, Git checkouts, and
archives: `.agents/skills/`, `.claude/skills/`, `.opencode/skills/`, and
`skills/`. An archive may carry those folders at its top level, or inside one
release wrapper directory such as `packed-1.0/`. An archive that contains one
bare skill directory (`only-skill/SKILL.md`), or only the skill files
themselves in an archive named after the skill (`only-skill.zip`), installs as
a single-skill source, exactly like passing the unpacked skill directory.

What happens during install:

1. `skillctl` inspects the source and finds skill candidates.
2. You select the exact skill name and scope, either explicitly or interactively.
3. `skillctl` updates the manifest and lockfile.
4. It stores a cached immutable copy of the selected skill subtree under `~/.skillctl/store/imports/`.
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

`path` answers for canonical local skills too. A skill you authored in
`.agents/skills/` has no install record, because the local state store only
records imports, so `path` resolves it from the workspace layout instead. Its
report names the canonical root and the planned runtime roots; the cached
import, overlay, and projection rows stay empty because none applies. A
generated copy inside `.agents/skills/` is not a canonical skill, so such a
name keeps the refusal, which names the root it checked and points at
`skillctl explain`.

`history` answers for the same skills. It is ledger-driven, so a canonical
local skill reports an empty ledger with exit code `0`:

```text
$ skillctl history canon
No history entries recorded for canon.
  canonical local skill at .agents/skills/canon
  no ledger events: sync projects it straight from that root; run 'skillctl explain canon' for its projections
```

`skillctl sync` records no projection event for a canonical skill, because the
ledger follows managed imports and `sync` materializes a canonical skill
straight from its root. A name the ledger does not cover at all keeps a
refusal that says so and points at `skillctl explain`. A cleanup event that
names a canonical skill still reaches the ledger, and `history` then lists it.

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

### A workspace without a manifest

The manifest is optional for the simplest local-only flow. A workspace that
holds canonical skills under `.agents/skills` but no `.agents/skillctl.yaml`
syncs on the documented defaults: the same targets `skillctl init` writes
(`codex`, `gemini-cli`, `opencode`), copy mode, and the default layout.

Because you never chose those targets, `sync` reports one warning that names
`skillctl init`, and `skillctl doctor` reports the same state as a
`manifest-missing` warning. `sync` itself never writes the manifest; run
`skillctl init` to create it and choose targets explicitly.

`sync`, `validate`, `explain`, `list`, `path`, and `doctor` run on those
defaults. `verify`, `update`, and `restore` do not: they answer from the
imports and overrides the manifest declares, and a workspace without one
declares nothing. They refuse with exit code `3` and a message that names
`skillctl init`, or the committed manifest in a team checkout, rather than
reporting an empty declaration as if it were yours.

`pin`, `rollback`, `override`, and `fork` rewrite an import declaration, so they
need the manifest too. Each command resolves the skill name first. A name that
no install record or declaration covers, such as a canonical local skill, gets
the same `is not installed` refusal that `enable`, `disable`, and `remove` give
it. A name that resolves through the local state store then refuses with the
message that names `skillctl init`, because the declaration to rewrite is what
the workspace lost.

A directory with no skills, no lockfile, and no manifest is not a workspace
that lost its manifest, so `doctor` stays quiet there and the bundled
user-scope skill keeps working without one.

### A workspace without a lockfile

The lockfile is never optional for commands that read or rewrite pins. A
workspace that holds `.agents/skillctl.yaml` but no `.agents/skillctl.lock`
refuses those commands with exit code `3`: `verify`, `update`, `restore`, `pin`,
`rollback`, `override`, and `fork`.

The refusal names two repairs, and they are not interchangeable:

- in a team checkout, retrieve the committed `.agents/skillctl.lock`
- run `skillctl init` only when the workspace pinned nothing yet

`skillctl init` writes an empty lockfile. An empty lockfile verifies zero
imports, so a workspace that lost a populated lockfile would look healthy with
no pins at all. That is why the refusal tells a team checkout to retrieve the
committed file instead.

`skillctl doctor` reports the loss itself as a `lockfile-missing` warning
whenever the manifest exists, and the warning keeps both repairs. The manifest
is the declaration you chose, and `init` writes both files, so a manifest
without a lockfile is a file the workspace lost, not a state it chose. The
warning also covers a manifest that declares no imports, where no per-import
error could carry the loss, and it appears beside the per-import errors when
the manifest declares imports.

A manifest that declares imports makes the loss visible to `sync`, `validate`,
`explain`, and `doctor` too: each reports
`enabled import '<name>' is missing from the lockfile`. `list`, `path`, and
`history` keep answering.

### Repeated syncs change nothing

`sync` is idempotent. A generated copy that already matches the planned content
and metadata stays byte-identical. Running `sync` again with no changes writes
nothing, reports that no generated projections were required, and appends no
history entries. A reused canonical root such as `.agents/skills` therefore
stays clean in a committed tree.

The report names the copies it skipped in `data.generated_roots[].unchanged[]`
and counts them in `data.unchanged_skills`.

### Roots that stop being planned

When you change the target set, a physical root can drop out of the plan. For
example, switching from the default targets, whose shared root is
`.agents/skills`, to `claude-code` alone leaves `.agents/skills` unplanned.

While `projection.prune` is on (the default), `sync` prunes the
skillctl-generated copies in every documented root the current plan no longer
includes, not only the roots it plans. Each prune is reported in
`data.pruned_unplanned_roots[]` and recorded in the local history. Hand-authored
directories are never touched, copies with unrecorded edits are preserved and
reported, and the bundled `skillctl` skill keeps its user-scope copies because
the bundled projection plan covers every documented user root.

A preserved copy in a dropped root is repaired by the same
`skillctl update <skill> --apply` strategies a planned root gets, because the
repair scans every documented root of the scope, not only the planned ones.

While `projection.prune` is off, `sync` leaves those copies in place and
`doctor` reports each one as a `stale-generated-projection` warning that names
the root no enabled target plans any more.

### Edits inside a generated copy survive sync

A generated copy records the content digest it was written with. When a copy no
longer matches that digest, `sync` keeps the copy, reports it as `preserved`
with the reason `dirty-projected-copy`, and exits with `2`:

```text
warning: preserved '.claude/skills/ai-sdk' with unrecorded edits (...)
```

The exit code `2` means "success with warnings". The command completed; the
edited copy was left alone.

To back the copy up and regenerate it, be explicit:

```bash
skillctl sync --discard
```

`--discard` copies the edited tree into `~/.skillctl/store/discards/` first.
The response reports the backup path, and the local ledger records the discard.

The repair advice that `doctor`, `explain`, `sync`, and `clean` print for the
copy follows the trust gate. A skill without scripts is told to run
`skillctl update <skill> --apply --strategy overlay` or `--strategy discard`,
because both resolve the edits whether an update moves the revision or the
import is already up to date.

An unreviewed skill that contains `scripts/` and tracks a Git source is told the
same two commands while no upstream revision waits for it, because the repair
then keeps the pinned revision and the gate holds back only applies that move an
import upstream. The local state settles that question two ways: the skill is
pinned to a commit or a recorded version, which no upstream ref can move on
from, or the newest recorded update check for the current pin reported nothing
to move to. Run `skillctl update <skill>` to record such a check.

A script-free import gets the same treatment, because the revision a repair
would move it to can introduce the `scripts/` directory the installed content
never had. A drift report never reads upstream, so it reads the answer from the
newest recorded update check instead: while that check saw a waiting revision
that adds `scripts/`, the advice turns to the commands below, because the
repair apply would be refused with exit code `4`. A check that saw a script-free
waiting revision keeps the update strategies, and so does a script-free import
no check has observed yet.

Without one of those answers the advice turns conservative and names the
commands that work in every state:

```bash
skillctl update <skill> --apply --strategy fork
skillctl sync --discard
```

The fork keeps the edits as local ownership, and the discard regenerates the
copy. A later revision that starts waiting re-arms the gate, so an advised
`--strategy overlay` or `--strategy discard` can still be refused with exit code
`4`; that refusal names the review and the fork that let it through. Record a
review with `skillctl review <skill> --approve` to license the move the check
held back, or run `skillctl update <skill>` again to observe the newer revision.

A skill no install record holds is told to run `skillctl sync --discard` alone.
This covers a copy left behind by `skillctl remove` and the generated copy of a
canonical local skill in `.agents/skills/`, because `update` refuses both names
with exit code `3` before any strategy can run.

The two update strategies also resolve a copy in a root the current plan no
longer includes, and the leftover copy of a disabled import. Both repairs read
the digest each copy records, which is the same evidence `sync` preserves the
copy on, so they work wherever `sync` reports the copy. A copy written before
digests were recorded is judged against the content of the pinned source
instead, so the same two strategies resolve it. A repair keeps a
disabled import disabled: the edits move into its overlay, and the next
`skillctl enable <skill>` projects them again.

### Symlink mode links each projection into its source

`projection.mode` decides what a generated copy holds. The default `copy` mode
writes one file per projected file. The opt-in `symlink` mode writes one symlink
per projected file:

```yaml
projection:
  mode: symlink
  allow_unsafe_targets:
    - claude-code
```

A canonical local skill links into `.agents/skills`, so editing the projection
edits the canonical file. That is the point of the mode: one file, one copy, no
duplication.

An imported skill links into `~/.skillctl/store/imports`, which holds the bytes
`.agents/skillctl.lock` pins. That store is content skillctl re-derives, not a
working tree you own. An editor that follows the link therefore writes the
pinned content itself:

```bash
printf '\n# local note\n' >> .claude/skills/ai-sdk/SKILL.md
```

The projection's own links are unchanged by such a write, so the copy still
hashes to the digest its metadata records. The divergence shows up one level
higher, where the store no longer hashes to the pin:

- `skillctl sync` keeps the projection, reports it as `preserved` with the
  reason `diverged-import-store`, and exits with `2`. It never drops the edit
  and never relinks the copy, because the links would serve the same changed
  bytes. `skillctl sync --discard` changes nothing here: the edit is not in the
  copy, so there is nothing to back up and regenerate.
- `skillctl verify` exits with `3` and reports that the import does not match
  the pinned content.
- `skillctl update <skill>` exits with `3` with the same mismatch, because the
  check resolves through the store it is asked to repair. No update strategy
  lifts this, so `skillctl update <skill> --apply --strategy fork` fails the
  same way: the apply runs its check through the store first. Its error and
  the sync report therefore name the repairs that work:

```bash
skillctl restore --frozen    # rebuild the pinned content from the retained artifact
skillctl install <source> --name ai-sdk --scope workspace    # re-pin the changed content
```

Both repairs rewrite the store, so both drop the local note. Run
`skillctl fork ai-sdk` first when you want to keep it: the fork copies the stored
bytes into a canonical skill you own, and the next `sync` links the projection
to that skill instead. The fork must be that standalone command, because it
reads the stored bytes directly instead of checking the broken baseline. This
is the symlink-mode limit of the rule that edits inside a generated copy
survive `sync`: the edit survives, but it lives in the import store, where
every repair rebuilds the pinned bytes.

The divergence belongs to the one import, not to the workspace. A run that
names the diverged skill refuses as described above, but a whole-workspace
`skillctl update` still checks every other skill: the diverged import reports a
`blocked` plan that carries the same two repairs, an `--apply` run skips it with
that reason, and the other imports update normally. The apply's final `sync`
keeps reporting the diverged copy, so the state stays visible until you repair
the store.

A target whose adapter documents unstable symlink behavior must also appear in
`projection.allow_unsafe_targets`, and every sync still warns that copy mode is
safer. Prefer `copy` unless you accept that trade.

### An edited overlay that the lockfile does not pin yet

The lockfile pins the digest of each overlay. When you edit an overlay file,
the directory on disk stops hashing to that pin. `sync` still projects the
edited content, because the edit is what you asked for, but it reports the
drift and exits with `2`:

```text
warning: import 'ai-sdk' has an overlay that hashes to sha256:… instead of the
pinned sha256:…, so this sync projects overlay content the lockfile does not
pin and a frozen restore refuses the pair. Run 'skillctl override ai-sdk' to
re-pin the change after reviewing it, then commit the lockfile together with
the overlay
```

Committing that state gives every teammate a checkout whose
`restore --frozen` fails on the digest mismatch. Review the edit, re-pin it,
and commit the lockfile with the overlay:

```bash
skillctl override ai-sdk
```

`override` never overwrites existing overlay files, so running it again only
records the new digest in the lockfile. The warning clears on the next `sync`.

While the overlay is unpinned, an update check still reads the generated copies.
It judges each copy against the content skillctl wrote for it, not against the
effective content. A copy you edited stays reported, and
`skillctl update <skill> --apply --strategy overlay` still repairs it. The check
warns about the overlay in the same run:

```text
warning: skill 'ai-sdk' has a managed overlay that changed since the recorded
state; run 'skillctl override ai-sdk' to re-pin the change after reviewing it,
then commit the lockfile together with the overlay
```

When one file holds both a copy edit and an unpinned overlay edit, a repair
keeps one of them. `overlay` and `fork` keep the copy edit, because preserving
an unmanaged edit is what they exist to do. `discard` backs the copy edit up and
projects the overlay. Review the overlay change before you run either.

## Restoring A Team Checkout

The lockfile is a portable contract. One teammate installs a skill and commits
`.agents/skillctl.yaml`, `.agents/skillctl.lock`, and `.agents/overlays/`. The
lockfile pins the digest of each overlay, but it does not carry the overlay
files, so the overlay directory is committable portable state next to the two
control files. Everyone else reconstructs the same content with:

```bash
git clone <repository>
cd <repository>
skillctl restore --frozen
```

`restore --frozen`:

- rebuilds the local import cache, which is a per-checkout cache and never
  travels with the repository,
- verifies the reconstructed content against the digests pinned in the lockfile,
- verifies each declared overlay against the overlay digest pinned in the
  lockfile,
- leaves `.agents/skillctl.lock` byte-for-byte unchanged,
- never selects a newer revision. If the pinned commit is unavailable upstream,
  the restore fails instead of taking the branch HEAD.

An overlaid import that arrives without its overlay directory cannot reproduce
the pinned effective version. The restore refuses it before it acquires any
content, and names the missing overlay directory together with its pinned
digest:

```text
the lockfile pins overlay content sha256:91d3... but the overlay directory
'/checkout/.agents/overlays/alpha' is absent from this checkout. Overlays are
committable portable state next to .agents/skillctl.yaml and
.agents/skillctl.lock, so commit the overlay for 'alpha' or remove its
'overrides.alpha' entry before restoring
```

Committing the overlay directory is the fix. A team that wants to keep an
overlay private can exclude `.agents/overlays/` from version control, but then
every teammate has to provide that directory themselves before they restore.

Check the result without changing anything:

```bash
skillctl verify
```

`verify` reports, separately for each import, whether the pinned content is
intact, whether a retained artifact exists, whether the import could be
reconstructed on this machine, whether upstream checks exist for the source
kind, and what local policy says. It writes nothing and never contacts a
source. An absent or mismatched overlay is reported there as
`portability.restorable: false` with the overlay directory and its pinned
digest.

`freshness.upstream_check` names one of three states:

- `available`: the source kind supports upstream checks, and
  `freshness.upstream_revision` is the last revision an operation observed,
- `unavailable-retained-snapshot`: a folder or archive import has no upstream,
- `unavailable-commit-pinned`: the manifest reference names an immutable
  revision, so no upstream ref can be observed for it
  (see [Pinning And Rolling Back](#pinning-and-rolling-back)).

### Where restore gets content from

Restore tries, in order:

1. the workspace import cache, when it still verifies,
2. a retained immutable artifact under `~/.skillctl/store/artifacts/`,
3. the locked revision acquired from its source.

Step 2 is why a moved checkout and an evicted cache still restore without any
network access:

```bash
skillctl restore --frozen --offline
```

Offline restore succeeds only from verified cached artifacts. When neither the
cache nor the artifact store holds the content, the command fails and says so
rather than pretending it checked upstream.

### What happens in each situation

| Situation | What `restore --frozen` does |
| --- | --- |
| Fresh clone of a repository | Acquires each locked revision from its source, verifies it, and syncs projections |
| Checkout moved to another path | The import cache is keyed by checkout path, so it is rebuilt from the retained artifacts or the source |
| Import cache evicted from `~/.skillctl` | Rebuilds from the retained artifacts; with `--offline` no source is contacted |
| Upstream moved on after the lockfile | Still restores the locked revision, never the new branch HEAD |
| Upstream no longer has the locked revision | Fails with the revision and the source, and changes nothing |
| Pinned overlay directory is not committed | Fails before acquiring anything, naming the overlay directory and its pinned digest |
| Overlay no longer matches its pinned digest | Fails rather than projecting content the lockfile does not pin; `skillctl override <skill>` re-pins it after review |
| Source is a folder or archive that is missing here | Fails with an explanation that the source is not portable, instead of guessing |
| Retained artifact was modified | Fails verification rather than trusting or overwriting it |

### Upgrading an old lockfile

Lockfiles written as version 1 recorded local timestamps inside each import.
They still load. `skillctl restore --frozen` leaves them untouched, and
`skillctl restore` upgrades them to the current version while recording the
canonical bundle digest of the content it verified. The upgrade never moves a
pinned revision or a content digest.

See [bundle-format.md](./bundle-format.md) for the canonical bundle format, the
hashing rules, and the lockfile fields.

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
skillctl override ai-sdk
skillctl sync
skillctl explain ai-sdk
```

The second `skillctl override ai-sdk` re-pins the edited overlay digest in the
lockfile. Skip it and `skillctl sync` exits with `2` to say the projected edit
is not pinned yet.

Important overlay rules:

- overlay paths must be portable relative paths
- overlay files must map to files that exist in the imported skill
- overlays are for imported skills, not for canonical local skills you already own

The bundled `skillctl` skill in user scope is locally owned, so it is not an
imported skill. `skillctl override skillctl` refuses it with exit code `3` and
names the exits: stop projecting it with `skillctl --scope user remove
skillctl`, or install a copy you control with `skillctl install <source>
--name skillctl --scope workspace` and overlay that import.

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

Without `--apply` the command is read-only: it records the check in the local
ledger and recommends a follow-up action.

You may see outcomes like:

- `apply`: safe to refresh the pinned revision
- `create-overlay`: local changes should move into `.agents/overlays/<skill>`
- `detach`: keep a full local copy instead of continuing as a managed import
- `skip`: keep the current pin for now

An import pinned to a commit gets outcome `commit-pinned` instead, because a
commit names no upstream ref the check could observe. See
[Pinning And Rolling Back](#pinning-and-rolling-back).

A well-known import tracks the artifact digest its discovery index declares.
Every check re-reads the index, so a changed digest is an update and an
unchanged one reports `up-to-date`. `update <skill> --apply` re-reads the index
again, refuses when it moved past the digest the check saw, and moves the
import to the artifact that digest verifies. `pin` and `rollback` stay
Git-only, because a discovery index declares no branch or tag to pin.

### A missing generated copy is not an edit

A destination without a generated copy leaves no local change behind, so
`update` never reports `local_modification_detected` or outcome `blocked` for
it. The check keeps the recommendation an intact workspace would get and names
every destination the manifest plan expects:

```json
"missing_projections": [
  {
    "targets": ["codex", "gemini-cli", "opencode"],
    "root": ".agents/skills",
    "path": ".agents/skills/ai-sdk"
  }
]
```

The plan decides which destinations the check expects, not the recorded
projection state, so a root the target set added before you ran `skillctl sync`
is named too. `skillctl explain` and `skillctl doctor` read the same plan, so
they report the same missing root for a managed skill. A root the plan dropped
is not named, because `sync` prunes that root instead of rebuilding it.

A forked skill gets the same naming. The fork moved the skill into local
ownership, but `skillctl sync` still materializes the local copy into every
planned destination, so a check that reports outcome `detached` also names the
destinations the workspace lacks.

The run warns and exits with `2`:

```text
warning: skill 'ai-sdk' has no projection at '.agents/skills/ai-sdk' for codex, gemini-cli, opencode; run 'skillctl sync' to materialize the missing projection
```

`skillctl sync` rebuilds the copy. `skillctl update ai-sdk --apply` rebuilds it
as part of the apply, so that apply does not repeat the warning; a run that
leaves the skill behind, such as `--strategy skip`, does. `skillctl explain
ai-sdk` observes the same destination as `observed_presence: "missing"`.

A `--plan` run warns the same way. The document changes nothing and rebuilds no
projection, so the warning names every skill the check covered, including one
that contributes no plan because its check was up-to-date or detached.

### Applying an update

```bash
skillctl update ai-sdk --apply
skillctl update ai-sdk --apply --strategy overlay
skillctl update --apply
```

An apply runs through one guarded engine that every mutating command shares:

1. it verifies a typed plan against the lockfile, the canonical bundle digest,
   and the install record before anything changes;
2. it moves the import to the new revision, recomputes the bundle digest,
   retains the artifact, and rebuilds the projections in one journalled
   transaction;
3. it records an `update-applied` history entry, so the previous revision stays
   a rollback point.

The previous revision stays recorded, so one command undoes an apply:

```bash
skillctl rollback ai-sdk <previous-revision>
```

### The trust gate refuses an unreviewed script-bearing import

An imported skill starts as `imported-unreviewed`. The gate evaluates the two
trees an update apply joins, so executable content cannot arrive unreviewed:

- the installed import: when it contains a top-level `scripts/` directory and a
  newer upstream revision exists, the update check reports
  `trust.blocked_actions: ["apply-update"]`;
- the incoming revision: when the installed content is script-free and the newer
  revision adds a top-level `scripts/` directory, the check reports the same
  block, because that revision holds executable content no recorded review
  covers.

```text
warning: trust gate is blocking update apply for 'ai-sdk' until it is reviewed or forked; record a local review with 'skillctl review ai-sdk --approve'
```

When the block comes from the incoming revision, the warning and the refusal
name it instead:

```text
warning: trust gate is blocking update apply for 'ai-sdk' until it is reviewed or forked; the incoming revision 4e6a597 adds a top-level scripts/ directory no recorded review covers; record a local review with 'skillctl review ai-sdk --approve'
```

A check that finds a waiting revision for an unreviewed import reads the
upstream tree before it reports, so it can name the executable content before
you apply anything. When the source cannot be read, the check still reports the
revision it observed and the apply takes the decision. The plan keeps reporting
the installed facts, so `trust.contains_scripts` stays `false` and
`trust.risk_level` stays `normal`; `trust.blocked_actions` is what carries the
block. The check also records why it held the apply back on its own row in
`~/.skillctl/state.db`, which is the evidence the drift-repair advice reads, see
[Edits inside a generated copy survive sync](#edits-inside-a-generated-copy-survive-sync).

An apply then refuses instead of warning. `skillctl update ai-sdk --apply` exits
with `4`, names the skill, and changes nothing: the pinned revision, the stored
import, and every generated copy stay as they are. `--strategy overlay` and
`--strategy discard` are refused too, because both move the import to the new
upstream revision.

The engine derives the gate from the tree it checks out, not only from the
decision the check recorded, so a plan document cannot install unreviewed
executable content either.

An unmanaged edit in a generated copy does not open a way past the gate. The
check then reports outcome `blocked` with the same `trust.blocked_actions`
entry, and a repair apply refuses the same way, because resolving the edits and
moving to the new revision happen in one apply. The edit stays in the generated
copy untouched.

`skillctl pin` and `skillctl rollback` move an import to a revision through the
same engine an update apply uses, so the same gate decides them. A pin or a
rollback that would leave an unreviewed script-bearing import on executable
content refuses with exit code `4` before anything changes, and the refusal
names the repair that fits an explicit revision move:

```text
trust gate is blocking pin for 'ai-sdk' until it is reviewed or forked; record a local review with 'skillctl review ai-sdk --approve', or take local ownership with 'skillctl fork ai-sdk'
```

A rollback prints the same refusal with `rollback` in place of `pin`. The three
commands cannot diverge, because they all consult one gate predicate inside the
apply engine. A pin or a rollback that names the revision the import already
holds changes nothing, so it stays ungated.

Two strategies stay open, because they are the documented way past the gate:

- `skillctl update ai-sdk --apply --strategy fork` takes local ownership of the
  current content in `.agents/skills/ai-sdk/`
- `skillctl update ai-sdk --apply --strategy skip` keeps the current pin

A fork keeps the installed content, so it does not deliver the revision that
adds scripts. Only a review does that.

A recorded review opens the third way past the gate. See
[Record a review of imported content](#record-a-review-of-imported-content).

MCP reports the same refusal: `skills_update_apply` and `skills_rollback`
return `isError: true` with `ok: false` and the same error text, and the CLI
exits with `4`.

### Record a review of imported content

`skillctl review` is the local, human decision that an import was read. It
moves a script-bearing import from `imported-unreviewed` to `imported-reviewed`,
which is the state the trust gate needs before an update apply may move it.

Report what is on record:

```bash
skillctl review ai-sdk
```

Record that the effective content was reviewed:

```bash
skillctl review ai-sdk --approve --note 'scripts inspected, safe'
```

Withdraw a recorded decision:

```bash
skillctl review ai-sdk --revoke
```

What a review changes:

- `doctor` stops reporting `script-risk` for the skill and stops warning about
  its scripts,
- the update check stops warning and recommends the apply the gate held back,
- `update --apply`, `--strategy overlay`, and `--strategy discard` work again,
  including for a revision that introduces `scripts/`, because the recorded
  decision licenses the move. `pin` and `rollback` move to such a revision too,
  so a recorded review is what unblocks a gated one. Read the incoming revision
  before you record the review, because the decision states what a person read.

What it does not change:

- The decision covers one effective version. An applied update, a pin, a
  rollback, a new overlay, or any other change to the effective content returns
  the import to `imported-unreviewed`, and a fresh review is needed.
- The decision is local state in `~/.skillctl/state.db` with a `trust-review`
  history entry. It is not written to `.agents/skillctl.lock`, so it does not
  travel with the repository.
- A review records that one person read one version of the content. It is not
  an organizational approval, a signature, or a scan result, and it does not
  travel with the repository.
- The command refuses a bundled, forked, or detached skill, because those are
  locally owned and need no review.

### Apply strategies

`--strategy` decides what happens to edits skillctl did not record:

| Strategy | Behavior |
| --- | --- |
| `clean` | Apply only when no unmanaged edits exist. Refuse with exit code `3` otherwise. |
| `overlay` | Move the edited files into `.agents/overlays/<skill>`, then apply. The edits survive the update. |
| `discard` | Copy the edited tree into `~/.skillctl/store/discards/`, then regenerate it from the new revision. |
| `fork` | Detach into `.agents/skills/<skill>` instead of applying. |
| `skip` | Keep the current pin and report the decision. |

`clean`, `overlay`, and `discard` all move the import to a new upstream
revision, so the trust gate refuses all three with exit code `4` while an
unreviewed script-bearing import has an update waiting. That holds whether or
not a generated copy holds unmanaged edits.

Every strategy runs after the update check, and the check reads the import
store the lockfile pins. A store that no longer hashes to its pin refuses the
run before any strategy acts, `fork` included, with exit code `3`. The repair
that keeps those bytes is the standalone `skillctl fork <skill>` command,
which reads the store directly instead of checking the broken baseline. See
[Symlink mode links each projection into its source](#symlink-mode-links-each-projection-into-its-source).

`clean` is the default.

Folder and archive imports have no upstream revision, so an apply for them
resolves the edits and keeps the pinned source content. `overlay` and `discard`
repair an edited generated copy, and `fork` still takes local ownership. A
requested apply that changes nothing exits with `2` and lists why each skill was
skipped.

An import whose check reports up-to-date gets the same treatment. Nothing
upstream moves, so `overlay` and `discard` repair an edited generated copy on
the pinned revision, and a plain `--apply` refuses with exit code `3` instead of
reporting a silent zero. `skillctl sync --discard` refreshes the copy too, but it
cannot keep the edits the way `overlay` does.

The same repair runs while the managed overlay is edited but not re-pinned. The
check judges each copy against the content skillctl wrote for it, so the edits
stay repairable, and the apply re-pins the overlay digest it recomputes. See
[An edited overlay that the lockfile does not pin yet](#an-edited-overlay-that-the-lockfile-does-not-pin-yet).

### Planning first, applying later

`--plan` writes a typed, immutable plan document without changing anything:

```bash
skillctl update --plan update-plan.json
```

`--plan-file` applies exactly that document:

```bash
skillctl update --apply --plan-file update-plan.json
```

Each plan carries a self digest. A document edited after it was written fails
with a `tampered` error, and a plan built against different local state fails
with a `stale` error. Both fail before any mutation, with exit code `3`.

The stale check runs before the trust gate. No review can make an outdated
document applicable, so a stale document reports the stale error even when the
import it names is gated. A gate refusal therefore names a review only while the
document is still current.

A skill whose edits block an update still contributes a plan. The document
records the revision move, or the edit repair for an import whose revision does
not move, and the apply that runs it chooses its own strategy, so `--plan`
writes that plan even for the default `clean` strategy and warns when the edits
need a repair strategy:

```text
warning: 'ai-sdk' holds unmanaged local edits that the clean strategy cannot resolve; run 'skillctl update ai-sdk --apply --strategy overlay' to keep the edits, or 'skillctl update ai-sdk --apply --strategy discard' to back them up and drop them
```

The run exits with `2`, so pick the strategy first and rebuild the document:

```bash
skillctl update ai-sdk --plan update-plan.json --strategy overlay
skillctl update --apply --plan-file update-plan.json
```

An import the trust gate blocks is advised the `fork` instead, because the gate
refuses every strategy that moves the revision.

A document that holds no plans, or none that name a managed skill, leaves the
apply nothing to do. That apply exits with `2` and says so instead of reporting
a silent zero.

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

A pin and a rollback are revision moves, so the trust gate decides them exactly
as it decides an update apply. While the import is unreviewed and holds
executable content — installed `scripts/`, or a target revision that introduces
them — the command refuses with exit code `4` and changes nothing. The refusal
names the repair that fits an explicit revision move: record a review with
`skillctl review ai-sdk --approve`, which licenses the move, or take local
ownership with `skillctl fork ai-sdk`. See
[The trust gate refuses an unreviewed script-bearing import](#the-trust-gate-refuses-an-unreviewed-script-bearing-import).

A gated rollback is never a dead end. The review it names covers the content
the workspace holds now, and the same rollback then succeeds, so this is the
recovery path back to a previous revision after an update you no longer trust:

```bash
skillctl review ai-sdk --approve
skillctl rollback ai-sdk <previous-revision>
```

The fork it names is the other exit: it takes the current content into
`.agents/skills/ai-sdk/` as a locally owned skill that no longer moves with
upstream at all.

A branch or tag keeps tracking upstream. A commit does not: a commit names one
immutable revision, and no upstream ref can answer for it. After
`skillctl pin ai-sdk 0123456789abcdef` or
`skillctl rollback ai-sdk 0123456789abcdef`, `skillctl update` reports outcome
`commit-pinned`, records no `latest_revision`, and explains how to resume:

```text
import 'ai-sdk' is pinned to the immutable revision 0123456789abcdef, which
names no upstream ref to observe; upstream tracking is unavailable while the
pin names a commit or a recorded version. Run 'skillctl pin ai-sdk
<branch-or-tag>' to resume upstream tracking
```

`skillctl verify` reports the same state as
`freshness.upstream_check: "unavailable-commit-pinned"` with no
`upstream_revision`, the same way it names an unavailable retained snapshot. The
check never reports the pinned commit as an upstream observation, so an import
pinned to a commit never reads as up to date merely because tracking froze.

Unmanaged edits in a generated copy stay repairable while the pin names a
commit. `skillctl update ai-sdk --apply --strategy overlay` keeps the edits and
keeps the pinned revision, exactly as it does for a folder or archive import.

Roll back to a previously recorded version or effective revision:

```bash
skillctl rollback ai-sdk 0123456789abcdef
skillctl rollback ai-sdk sha256:effective-version
```

A rollback records the same commit-pinned state, because the rollback target
names one immutable revision too.

A check recorded before the move still names the revision the install left as
the newest one it saw. `skillctl list` and `skillctl tui` fit that stored check
to the revision the install holds now, so they report `update-available` with
the recorded revision instead of calling a rolled-back install up to date. Run
`skillctl update <skill>` to record a fresh check.

A rollback keeps the overlay you have now. The revision moves, the overlay
stays, and `skillctl` records the effective version that results. This is the
same transition `skillctl pin` performs for that revision, so an overlay you
created after an update never blocks a rollback to the revision that predates
it.

A version the local history does not record is refused, and the refusal names
the command that still moves there:

```text
version '0123456789abcdef' is not recorded for skill 'ai-sdk'; run
'skillctl pin ai-sdk 0123456789abcdef' to move this import to a revision the
local history does not record
```

That `pin` crosses the same trust gate a rollback does, so for an unreviewed
script-bearing import it can refuse with exit code `4` too. Its refusal names
the review or the fork, which are the same repairs a gated rollback names.

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
- a symlink-mode edit wrote the import store, and you want to keep those bytes
  before a repair rebuilds it. This fork must be the standalone command, because
  `skillctl update <skill> --apply --strategy fork` still runs its check through
  the diverged store and refuses.

Choose `override` when:

- you only need a small managed customization layer
- you still want to track and update upstream cleanly

After a fork, `skillctl update <skill>` reports outcome `detached` and
recommends `skip`, because no upstream is left to check. The check still reads
projection health: a destination without a generated copy is named in
`missing_projections` with the same `skillctl sync` advice a managed import
gets.

A detached skill never blocks an apply. Its ownership state is not an edit, so
`skillctl update --apply` skips it under every strategy and warns when the run
changed nothing. The `clean` refusal names only a skill whose generated copies
hold unmanaged edits.

Fork needs a managed import too. The bundled `skillctl` skill in user scope is
locally owned, so `skillctl fork skillctl` refuses it with the same exits as
override: stop projecting it with `skillctl --scope user remove skillctl`, or
install a copy you control as a workspace import and fork that import.

## Enabling, Disabling, Removing, And Cleaning

These commands solve different lifecycle problems.

Disable a managed import without deleting its cached source or history:

```bash
skillctl disable ai-sdk
skillctl enable ai-sdk
```

Disabled imports stay on disk but stop participating in resolution and projection until re-enabled. `skillctl explain <skill>` then reports the skill as not visible with a `disabled-import` warning, and `skillctl doctor` reports the same warning for the workspace, so a report with zero warnings always means every declared import projects. Because a disabled import keeps its manifest declaration and its lockfile pin, that pin is not reported as `stale-lockfile-entry`.

Enable and disable resolve the import through the local state store first. When no install record exists, they fall back to the import the committed manifest and lockfile declare. Both therefore work on a checkout that has not restored its import cache yet. Disabling an unrestored import keeps the pin and prunes its generated copies, which makes `skillctl sync` usable again. Enabling one keeps the pin too. The projection rebuild is then skipped with a warning until you restore the source or replace the import.

Disable never touches the bundled user-scope skill. It is locally owned, so it carries no manifest import to toggle. `skillctl disable skillctl` and `skillctl --scope user disable skillctl` both refuse it and name `skillctl --scope user remove skillctl` as the supported exit. `skillctl --scope user enable skillctl` still reinstalls it. When a committed workspace import uses the bundled skill's own name, an unscoped disable reports the shared scope-resolution error first, and `--scope workspace` reaches the import.

An unscoped `enable skillctl` also refuses when a lockfile pin shares the bundled name and the manifest no longer declares it. Enabling the bundled skill would report success for a name the stale pin still claims. Re-run with `--scope user` to enable the bundled skill, or clear the entry with `skillctl --scope workspace remove skillctl`.

Override and fork never act on the bundled user-scope skill either, because both need a managed import. `skillctl override skillctl`, `skillctl fork skillctl`, and their `--scope user` forms refuse it and name the exits: `skillctl --scope user remove skillctl` to stop projecting it, or `skillctl install <source> --name skillctl --scope workspace` to manage a copy those commands can act on. When a committed workspace import uses the bundled skill's own name, an unscoped override or fork keeps the shared scope-resolution error and names the `--scope workspace` command that reaches the declared import.

Override and fork resolve the import through the local state store first and fall back to the import the committed manifest and lockfile declare. Both commands copy content out of the stored import source and update the local install record, so a checkout that has not restored its import cache refuses them and names the state that blocks the command:

```text
error: import 'ai-sdk' has no stored source in '~/.skillctl/store/imports/…'.
The import cache is local to each checkout, so reconstruct it with 'skillctl
restore --frozen', or clear the import with 'skillctl remove ai-sdk', before
override can use it
```

Run the restore first, then override or fork the import:

```bash
skillctl restore --frozen
skillctl override ai-sdk
```

An import whose source this checkout cannot reconstruct again has no restore path, so clear it with `skillctl remove ai-sdk` and install a portable replacement.

A checkout that holds the stored source but no install record gets the matching refusal and the same repair. `restore --frozen` verifies the cached content against the pinned digests and records the install again, so it needs no source access in that state.

Remove a managed skill entirely:

```bash
skillctl remove ai-sdk
skillctl --scope user remove skillctl
```

Remove deletes managed imports, cached stored sources, and generated projections for the selected skill. It does not delete canonical local skills, and it generally preserves overlays unless the detached local copy itself is being removed.

Remove resolves the skill through the local state store first. A checkout that holds only the committed manifest and lockfile has no install record yet, so remove falls back to the import those two files declare. A fresh clone can therefore replace an import it cannot restore:

```bash
skillctl remove ai-sdk
skillctl install https://github.com/acme/skills.git --name ai-sdk
```

A lockfile entry the manifest no longer declares is the `stale-lockfile-entry`
state `doctor` reports, and remove clears it in one step. The lockfile pins
imports by name without a scope, so such an entry resolves as a workspace
import: run the removal without a selector or with `--scope workspace`, because
`--scope user` refuses it. Remove reports `data.resolved_from` as
`lockfile-only` and warns that no install record or manifest import existed.
The same single command clears an entry that still has an install record but no
manifest declaration. Enable and disable keep refusing that entry, because they
have no manifest declaration to flip.

A name that exists in both scopes is never guessed. When the same name is installed at user scope and workspace scope, or a committed workspace import uses the bundled skill's own name `skillctl`, remove fails with the shared resolution error instead of deleting one of them. Select the scope explicitly:

```bash
skillctl --scope user remove skillctl
skillctl --scope workspace remove skillctl
```

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
- disabled imports
- projection drift
- wrong precedence roots
- trust warnings
- detached or forked state
- missing cached import state

When you are unsure which one to run, start with `doctor`.

### Local state recovery

`doctor` also reports recoverable local state, such as an operation that was
interrupted mid-run. Preview the recovery first, then apply it:

```bash
skillctl doctor --repair-state
skillctl doctor --repair-state --apply
```

The preview changes nothing. The apply step restores interrupted operations to
the state before they started and recovers schema-v1 rows whose owning
workspace is unambiguous. Rows that could belong to more than one workspace are
never changed; they are reported so you can resolve them by hand.

The same entry point handles a state database SQLite cannot read, for example
after another program truncates `~/.skillctl/state.db`. The apply step moves
the corrupt file aside, keeps it, and rebuilds an empty database. Run
`skillctl restore --frozen` afterwards to rebuild the import records from the
lockfile. A plain `skillctl doctor` keeps answering while the database is
unreadable: it reports one `state-database-unreadable` error issue in a partial
report instead of failing, and skips the checks that read local state. See
[troubleshooting](./troubleshooting.md) for the full recovery model.

### Observed state, not intended state

`explain` and `inspect` report what is actually on disk for each target, not
what the resolver intends. Each entry under `data.targets` carries:

- `observed_presence`: `present`, `missing`, `not-a-directory`, or `unreadable`
- `observed_matches_winner`: whether the observed content matches the active
  winner, when one exists and the projection is observable
- `observed_digest` and `expected_digest`: content digests of the observed
  projection and the active winner
- `observed_preserved_reason`: why a plain `sync` preserves the copy instead
  of refreshing it, when the copy holds content `skillctl` did not record
- `fix`: the repair step, when the destination is not healthy

`visible` now means "this target currently materializes the active winner". A
missing or stale projection reports `visible: false` even when a winner exists.
Human output includes the same repair steps, for example:

```text
warning: [claude-code]: target 'claude-code' has no projection at '.claude/skills/release-notes'; fix: run skillctl sync to materialize the missing projection
```

The winner can live outside the scope the command read. `skillctl explain
ai-sdk` without `--scope` reads workspace scope, so a skill that exists only as
a user-scope import resolves to a winner with `scope: "user"` while
`data.scope` keeps reporting `"workspace"`. The targets then report absence
from workspace scope, not a gap a plain `sync` could close. Each one keeps its
workspace root and `observed_presence`, stays `visible: false`, explains the
absence in `reason`, and names `skillctl --scope user explain ai-sdk` or
`skillctl --scope user sync` in `fix`, because those are the commands that can
change what the report sees. `doctor` reads the same scope, so it reports
nothing for the skill either. `inspect` labels the preflight with the
candidate's own scope for the same reason.

A generated copy that survives in a scope its winner does not live in, for
example a workspace copy of a user-scope import, is reported as
`stale-generated-projection` on both surfaces. `explain` carries the same fix
`doctor` reports, so the two commands agree on the copy and on the
`skillctl sync` that prunes it.

The bundled `skillctl` skill has no candidate in the workspace graph, because
`skillctl` installs it at user scope only. An unscoped `explain` or `inspect`
still reports that absence from workspace scope, sets `data.owner_scope` to
`"user"`, and names `skillctl --scope user explain skillctl` in the summary and
in every per-target `fix`, because that is the command that reads the
projections the bundled skill does plan. A workspace that resolves the name
itself, for example an import named `skillctl`, keeps its own report.

`doctor` separates three per-target conditions: `missing-projection` (nothing
is materialized), `blocked-projection` (the planned path is not a readable
skill directory), and `projection-drift` (a copy exists but does not match the
winner). A fourth code, `stale-generated-projection`, reports a generated copy
of a skill that no longer resolves, for example after the last import was
removed or disabled while `projection.prune` was off, or a generated copy left
in a documented root no enabled target plans any more. `explain` reports the
same code instead of claiming the skill is simply not visible.

### Exit codes and the JSON `ok` field agree

When `explain` or `inspect` reports a same-name conflict, the JSON payload
sets `ok: false`, the process exits with code `3`, and the MCP tool result
sets `isError: true`. The three surfaces describe the same outcome.
Diagnostics that succeed while reporting an unhealthy workspace exit with
code `2` and keep `ok: true`; treat code `2` as "success with warnings" and
inspect `data.issues`.

### JSON contract changes (2026-11-05)

These changes keep old fields and add new ones:

- `data.targets[]` gained `observed_presence`, `observed_matches_winner`,
  `observed_digest`, `expected_digest`, and `fix`. All are additive.
- `data.targets[].visible` changed meaning from "a winner was selected" to
  "the winner is materialized at the planned root". Consumers that used
  `visible` to detect winner selection should read `data.status` instead.
- `explain` and `inspect` now return `ok: false` with exit code `3` on
  same-name conflicts. Consumers that treated a conflict payload as
  successful should branch on `data.status == "conflict"`.
- `doctor` gained `data.adapters[]` with the verification date and
  documentation sources for each runtime adapter.
- `doctor` gained the `stale-generated-projection` issue code for a generated
  copy left in a planned root after its skill stopped resolving, and for a
  copy left in a documented root no enabled target plans any more.
- `sync` gained `data.pruned_canonical_roots[]`. It appears only when a reused
  canonical root, such as `.agents/skills` under the default targets, had a
  stale generated copy pruned.
- `sync` gained `data.pruned_unplanned_roots[]`. It appears only when a
  documented root the plan stopped including, such as `.agents/skills` after
  the target set moved to `claude-code`, had a stale generated copy pruned.
- Projection roots changed for some multi-target combinations after the
  2026-11-05 adapter re-verification; see
  [the development guide](./development.md#adapter-verification-log) for the
  exact planning consequences.

### JSON contract changes (2026-11-06)

These changes keep old fields and add new ones:

- `explain` and `inspect` targets gained `observed_preserved_reason`. It
  appears only when the observed copy holds content `skillctl` did not
  record, so a plain `sync` preserves the copy instead of refreshing it. The
  value is the same identifier `sync` reports under `data.preserved[].reason`,
  for example `dirty-projected-copy`.
- `sync` gained `data.unchanged_skills` and `data.generated_roots[].unchanged[]`.
  They report the generated copies that already matched the plan and were left
  untouched. Those copies no longer restamp `generated_at` and no longer append
  `projection` history entries, so a repeated `sync` over an unchanged
  repository leaves the working tree and the ledger alone.
- `review` reports `data.trust`, `data.review`, and
  `data.review_covers_current_content`. Every trust payload that carries a
  state (`doctor` issues, `inspect`, `update` plans) gained `trust.review`
  when a recorded approval covers the content, and `trust.effective_state` can
  now report `imported-reviewed`. A `trust-review` history kind exists.
- `explain` and `inspect` no longer report `stale-lockfile-entry` for an
  import the manifest declares with `enabled: false`. The lockfile pin that a
  disabled import keeps is not stale, so only `disabled-import` fires. Its fix
  names `skillctl enable <skill>` and `skillctl remove <skill>`. The
  `stale-lockfile-entry` fix on every surface names the `install` and `remove`
  pair that clears the entry, instead of inviting a hand edit of
  `.agents/skillctl.lock`.
- `remove` gained the `resolved_from` value `lockfile-only`. It appears when
  the lockfile pins an import that no install record or manifest declaration
  covers, which `remove` now clears in one step instead of requiring a
  reinstall first.
- `explain` and `inspect` report absence from the requested scope when the
  winner lives in another one. `data.scope` keeps naming the scope that
  planned the roots, `data.winner.scope` and `data.candidate.scope` name the
  scope that owns the skill, `visible` stays `false`, and `fix` names
  `skillctl --scope <winner-scope> explain <skill>` or
  `--scope <winner-scope> sync` instead of a plain `skillctl sync` that no run
  in the requested scope can satisfy. A generated copy left under such a
  winner reports `stale-generated-projection`, the code `doctor` reports for
  the same copy, and the human `inspect` summary labels the preflight with the
  candidate's own scope.
- `stale-lockfile-entry` no longer fires for an import the manifest declares at
  another scope. The lockfile pins imports by name without a scope, so a
  user-scope import owns the entry an unscoped `explain` reads. A disabled
  import of that kind reports `disabled-import` with `scope: "user"` and a fix
  that names `skillctl --scope user enable <skill>` and
  `skillctl --scope user remove <skill>`.

### JSON contract changes (2026-11-07)

These changes keep old fields and add new ones:

- `explain` and `inspect` gained `data.owner_scope`. It appears only when no
  candidate resolves in the requested scope because the bundled `skillctl`
  skill owns the name in user scope, which is the state an unscoped request
  finds after any command bootstrapped that skill. The value is `"user"`, and
  consumers that branch on `data.status == "missing"` can read it to learn
  where the name does resolve. A name the graph resolves, or a name the report
  carries a workspace finding for, keeps the ordinary payload without the
  field.
- The human summary sentence this change prints is renderer output. The
  `--json` envelope carries no `summary` field, so scripts must read
  `data.status` and `data.owner_scope` instead of parsing prose.
- `doctor` gained the `disabled-import` issue code for every import the
  manifest declares with `enabled: false`. The code already existed on
  `explain` and `inspect`; `doctor` now reports it with the same fix, so a
  doctor run with zero warnings no longer disagrees with an `explain` that
  exits `2` for the same skill. A doctor run that reports it exits `2`.
- `verify`, `update`, and `restore` refuse with exit code `3` when the
  workspace holds no `.agents/skillctl.yaml`. They previously exited `1` with
  the raw read error. The JSON envelope keeps `ok: false` and carries the
  refusal in `errors[]`, which names `skillctl init` as the repair. A
  checkout that holds a lockfile but no manifest hears that refusal instead
  of a report computed from an invented empty declaration.
- `pin`, `rollback`, `override`, and `fork` refuse with exit code `3` in a
  workspace that holds no `.agents/skillctl.yaml`. They previously exited `1`
  with the raw read error. Each command resolves the skill name first, so the
  refusal depends on the name. A name that no install record or declaration
  covers gets the same `is not installed` resolution error that `enable`,
  `disable`, and `remove` give it, and a name the local state store covers gets
  the refusal that names `skillctl init`.
- `history <skill>` gained `data.source`, `data.canonical_root`, and
  `data.note`. They appear only when no install record and no ledger row
  covers the name and the workspace skills root holds an authored canonical
  skill, which is the state that previously refused with exit code `3`. That
  report exits `0` with an empty `data.entries[]`, and a name a ledger row
  covers keeps the ordinary payload without the fields.

### Projection metadata timestamp format (2026-11-06)

`.skillctl-projection.json` stores `generated_at` as deterministic UTC RFC3339,
for example `2026-02-02T02:47:36Z`. That is the format the local state store
already uses for `generated_at`, so one field name now has one format.

Releases before this change stored unix seconds, for example `1770000456`.
Consumers that parsed the old value as a number should parse RFC3339 instead.

A copy stays current when only `generated_at` and `version` differ from what
this build would record: `generated_at` exists in both formats in the wild and
`version` records the build that last wrote the copy. An upgrade alone
therefore does not rewrite committed projections. Content drift still does,
because the remaining metadata fields, including `content_digest`, must match.

### Projection metadata upstream revision (2026-11-06)

`source.upstream_revision` in `.skillctl-projection.json` reports the last
revision an operation observed upstream. An import whose reference names one
immutable revision observes nothing upstream, so the field reads `null` for it.

Releases before this change copied the lockfile's recorded
`imports.<id>.revision.upstream` into that field, so a commit pin could read as
if upstream had been observed at the pinned commit. A lockfile written by such a
release may still carry that value; reads withhold it the same way
`skillctl verify` does, and the next pin, rollback, or apply rewrites the entry
without it. Because the field is part of what makes a copy current, every
commit-pinned copy re-materializes once after this change.

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
- review decisions
- cleanup events
- telemetry consent changes

If a skill behaves differently than you expected, `history` usually tells you when the change happened.

The ledger covers managed imports. A canonical local skill in `.agents/skills/`
never enters it, because `sync` projects such a skill straight from its root
and records no install or projection event, so `skillctl history <skill>`
answers for it with an empty report that names the canonical root and points
at `skillctl explain <skill>` for its projections.

## Default Human Output

Every command prints one summary line. `list`, `path`, and `history` also print one detail line per row they found:

```text
$ skillctl list
Listed 2 managed skills.
  ai-sdk [workspace] git 0123456789abcdef
    source: https://github.com/acme/skills.git (skills/ai-sdk)
    projections: codex, gemini-cli, opencode
  skillctl [user] built-in 0.1.0
    source: builtin://skillctl
    projections: amp, claude-code, codex, gemini-cli, github-copilot, opencode
```

```text
$ skillctl path ai-sdk
Resolved filesystem paths for ai-sdk.
  scope: workspace
  source: git https://github.com/acme/skills.git (skills/ai-sdk)
  revision: 0123456789abcdef
  canonical: none
  stored source: /home/you/.skillctl/store/imports/workspace/1f.../ai-sdk
  active source: /home/you/.skillctl/store/imports/workspace/1f.../ai-sdk/skills/ai-sdk
  overlay: .agents/overlays/ai-sdk
  planned root (codex): .codex/skills/ai-sdk
  projection (codex): .codex/skills/ai-sdk
```

A canonical local skill that was never installed reports the same shape with
the import-specific rows empty:

```text
$ skillctl path release-notes
Resolved filesystem paths for release-notes.
  scope: workspace
  source: canonical-local
  canonical: .agents/skills/release-notes
  stored source: none
  active source: none
  overlay: none
  planned root (codex): .agents/skills/release-notes
  planned root (gemini-cli): .agents/skills/release-notes
  planned root (opencode): .agents/skills/release-notes
```

```text
$ skillctl history ai-sdk
Showing 4 history entries for ai-sdk.
  2026-03-19T11:01:00Z  projection  ai-sdk  -  Projected ai-sdk into codex
  2026-03-19T11:00:00Z  install  ai-sdk  0123456789abcdef  Installed ai-sdk at 0123456789abcdef
```

Each history line lists the time, the event kind, the skill, the revision the event recorded, and the recorded summary. An event that carries no revision lists `-`.

The detail lines come from the same payload `--json` returns, so the two views report the same facts. Use `--quiet` to suppress human output; only `--json` returns data. Use `--verbose` to append the full JSON payload.

## JSON Output, TUI, And MCP

Every CLI command supports `--json`, which returns the normal response envelope:

```bash
skillctl --json list
skillctl --json path ai-sdk
skillctl --json doctor
```

This is the best mode for scripts, editors, and tooling.

`skillctl --json list` reports one `latest_update` object per skill. It holds the newest recorded check fitted to the installed revision. A check that ran before `skillctl update <skill> --apply` therefore reports `up-to-date`. A check for a revision the install does not hold still reports `update-available`. That includes a check that certified a revision the install later left through `skillctl rollback` or `skillctl pin`: the stored row still names the newest upstream revision it saw, so the fitted view reports that revision as available instead of certifying one the install no longer holds.

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

The shipped v1 tools mirror the CLI closely, such as `skills_list`, `skills_discover`, `skills_install`, `skills_update`, `skills_history`, `skills_override_create`, and `skills_doctor`. `skills_discover` reads a remote well-known discovery index read-only; it takes a required `url` argument and returns the same envelope as `skillctl discover <url> --json`. An agent reproduces a committed checkout with `skills_restore`, which requires an explicit `frozen` boolean because `frozen=false` may rewrite `.agents/skillctl.lock`, and checks the result with `skills_verify`.

## Telemetry

Telemetry is local-first and conservative by design.

Key points:

- local history is always kept
- telemetry is enabled by default after a first-run notice
- install and update events are classified locally as public, local-only, or private
- nothing is transmitted today because no emission backend exists
- skill contents, private repository identifiers, and arbitrary local paths never leave your machine

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
