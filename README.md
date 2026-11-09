# skillctl

[![CI](https://img.shields.io/github/actions/workflow/status/nibzard/skillctl/ci.yml?branch=main&label=ci)](https://github.com/nibzard/skillctl/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/nibzard/skillctl?label=release)](https://github.com/nibzard/skillctl/releases/latest)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](./LICENSE)

`skillctl` is a local-first skill manager for the open `SKILL.md` ecosystem.

It gives you one clean workflow for:

- keeping canonical workspace skills in one place
- installing skills from Git, local directories, archives, or well-known discovery indexes
- projecting them into the roots different agent runtimes actually read
- understanding what is active, what changed, and what is safe to update

If you use more than one coding agent, `skillctl` is the missing control plane.

## Why

Without `skillctl`, skill setups usually drift into some mix of duplicated files, ad-hoc symlinks, unclear precedence, and no reliable history.

`skillctl` fixes that by managing:

- canonical local skills in `.agents/skills/`
- imported skills pinned in `.agents/skillctl.lock`
- overlays in `.agents/overlays/`
- generated runtime-visible projections
- local history, diagnostics, and locally classified telemetry

Important rule:

- edit canonical skills or overlays, not generated runtime copies

## Install

Install the latest release:

```bash
curl -fsSL https://github.com/nibzard/skillctl/releases/latest/download/skillctl-install.sh | bash
```

Install a specific version or custom destination:

```bash
curl -fsSL https://github.com/nibzard/skillctl/releases/latest/download/skillctl-install.sh | \
  SKILLCTL_VERSION=v0.1.0 SKILLCTL_INSTALL_DIR="$HOME/.local/bin" bash
```

Published release artifacts currently cover:

- Linux `x86_64`
- macOS `x86_64`
- macOS `arm64`
- Windows `x86_64` as a zip download

Build from source:

```bash
cargo build --release --locked
./target/release/skillctl --help
```

## Quickstart

Initialize a workspace:

```bash
skillctl init
skillctl sync
```

Install a skill from Git:

```bash
skillctl install https://github.com/acme/skills.git --name ai-sdk --scope workspace
```

Or install from a local directory:

```bash
skillctl install ../shared-skills --interactive
```

Inspect what won and where it projects:

```bash
skillctl explain ai-sdk
skillctl path ai-sdk
skillctl doctor
```

Customize without forking the whole source:

```bash
skillctl override ai-sdk
$EDITOR .agents/overlays/ai-sdk/SKILL.md
skillctl override ai-sdk
skillctl sync
```

The second `skillctl override ai-sdk` re-pins the edited overlay in
`.agents/skillctl.lock`. Without it, `skillctl sync` still projects the edit but
exits with `2` and warns that the lockfile pins the previous overlay digest.

Update or pin later:

```bash
skillctl update ai-sdk --apply
skillctl pin ai-sdk main
skillctl rollback ai-sdk <version-or-commit>
```

Local edits in a generated copy are never silently dropped. `skillctl sync`
preserves them and says so. To keep them and still update:

```bash
skillctl update ai-sdk --apply --strategy overlay
```

That promise holds for the default copy mode. In symlink mode an edit that
follows the link writes the stored import itself, so read
[Projection Modes](#projection-modes) before you enable it.

## Default Layout

After `skillctl init`:

```text
repo/
  .agents/
    skills/
    overlays/
    skillctl.yaml
    skillctl.lock
```

The local state store lives in:

```text
~/.skillctl/state.db
```

Imported source snapshots are cached under:

```text
~/.skillctl/store/imports/
```

Immutable retained bundles live under:

```text
~/.skillctl/store/artifacts/
```

Durable backups taken before an explicit discard live under:

```text
~/.skillctl/store/discards/
```

## Projection Modes

`projection.mode` in `.agents/skillctl.yaml` decides what a generated copy
holds:

- `copy` (default): every projected file is a copy of its source.
- `symlink`: every projected file is a symlink to its source. Canonical skills
  link into `.agents/skills`. Imported skills link into the import store,
  which holds the bytes the lockfile pins.

Symlink mode is opt-in. List every target with unstable symlink support under
`projection.allow_unsafe_targets`, and `skillctl sync` still warns that copy
mode is safer:

```yaml
projection:
  mode: symlink
  allow_unsafe_targets:
    - claude-code
```

Symlink mode has one hazard copy mode does not have. A projected file of an
import links into the import store, so an editor that follows the link rewrites
the pinned content:

```bash
printf '\n# local note\n' >> .claude/skills/ai-sdk/SKILL.md   # writes the store
```

After that, `skillctl verify` and `skillctl update ai-sdk` fail with a content
hash mismatch, and `skillctl sync` keeps the projection, reports it as a
`diverged-import-store` preserved copy, and exits with `2`. The sync never
drops the edit and never repairs the store by itself, and
`skillctl update ai-sdk` cannot repair it because it resolves through the same
store. No update strategy lifts this, so `skillctl update ai-sdk --apply
--strategy fork` fails the same way. Repair the store with one of the commands
the error names:

```bash
skillctl restore --frozen    # rebuild the pinned content
skillctl install <source> --name ai-sdk --scope workspace    # re-pin the change
```

Both repairs drop the local note. Run `skillctl fork ai-sdk` first when you
want to keep it: the fork copies the stored bytes into a canonical skill you
own. The fork must be that standalone command, because an update apply runs
its check through the store first. An edit made through a link lives in the
import store, and every repair rebuilds the store.

The divergence blocks only `ai-sdk`. A plain `skillctl update` still checks the
other skills: `ai-sdk` reports a `blocked` plan, an `--apply` run skips it with
the same repair advice, and the rest update normally.

## Team Reproducibility

Commit `.agents/skillctl.yaml`, `.agents/skillctl.lock`, and `.agents/overlays/`.
The lockfile pins the digest of each overlay, but it does not carry the overlay
files, so the overlay directory travels with the repository. Everyone else gets
the same pinned skills with:

```bash
git clone <repository>
cd <repository>
skillctl restore --frozen
skillctl verify
```

`restore --frozen` rebuilds the local cache, verifies the content against the
pinned digests, and leaves the lockfile unchanged. It never substitutes the
branch HEAD for a locked revision. When the lockfile pins overlay content that
the checkout does not carry, the restore fails before it acquires anything and
names the missing overlay directory with its pinned digest. An overlay that no
longer hashes to its pin is refused the same way. `skillctl sync` reports that
drift instead of projecting it silently. The bundle format and hashing rules
are specified in [docs/bundle-format.md](./docs/bundle-format.md).

## Well-Known Discovery

A site can publish its skills in a discovery index at
`/.well-known/agent-skills/index.json`. Read it before you install:

```bash
skillctl discover https://skills.example.com
```

The report is read-only. It lists each declared skill with its resolved
artifact URL and digest, and it changes no local state. Install one entry as a
normal managed import:

```bash
skillctl install https://skills.example.com --name code-review --scope workspace
```

The boundary is deliberate. `skillctl` manages local skills in `.agents/skills`
and pins them in `.agents/skillctl.lock`. A discovery index is one way to find
and verify remote skills. It is not the storage model. `skillctl` reads
indexes. It does not publish, host, or mirror them. See
[docs/user-guide.md](./docs/user-guide.md) for the origin-trust policy and the
decision that publishing support stays out while the draft RFC has no stable
schema and few live publishers.

## Core Commands

| Command | What it does |
|---|---|
| `skillctl init` | Create the workspace manifest, lockfile, and default layout |
| `skillctl discover <url>` | Read the well-known discovery index an origin publishes, without installing |
| `skillctl install <source>` | Detect and install skills from Git, local paths, archives, or a well-known discovery index |
| `skillctl sync` | Rebuild generated runtime projections |
| `skillctl restore --frozen` | Reconstruct pinned imports from the lockfile and verify them |
| `skillctl verify` | Report integrity, portability, and freshness without changing anything |
| `skillctl update [skill]` | Check upstream state and recommend a safe next action |
| `skillctl update [skill] --apply` | Apply a planned update through the guarded apply engine |
| `skillctl list` | Show managed installs, projections, and current state |
| `skillctl explain <skill>` | Show the active winner, shadowed candidates, and visibility |
| `skillctl path <skill>` | Show canonical, cached, overlay, planned, and projected paths |
| `skillctl doctor` | Diagnose drift, conflicts, shadowing, and trust issues |
| `skillctl history [skill]` | Show installs, updates, rollbacks, overlays, and cleanup events |
| `skillctl override <skill>` | Create or reuse an overlay for a managed import |
| `skillctl fork <skill>` | Copy an imported skill into local canonical ownership |
| `skillctl review <skill>` | Show or record a local review decision for an import |
| `skillctl telemetry status` | Inspect current telemetry settings |
| `skillctl tui` | Open the read-only terminal dashboard |
| `skillctl mcp serve` | Run the MCP server with v1 tool parity |

Every command also supports `--json`. The default human output of `list`, `path`,
and `history` lists one line or block per row it found. Use `--quiet` to suppress
human output; only `--json` returns data.

## Supported Runtimes

`skillctl` currently plans projections for:

- Codex
- Claude Code
- GitHub Copilot
- Gemini CLI
- Amp
- OpenCode

Default workspace targets are:

- `codex`
- `gemini-cli`
- `opencode`

Add more in `.agents/skillctl.yaml` when needed.

## Typical Workflow

1. Keep local first-party skills in `.agents/skills/`.
2. Install third-party skills with `skillctl install`.
3. Let `skillctl sync` project the active result into runtime roots.
4. Use `skillctl explain` and `skillctl doctor` when behavior is unclear.
5. Record what you read with `skillctl review <skill> --approve` when an import holds scripts.
6. Use `override` for small changes and `fork` when you want full ownership.

## Telemetry

Telemetry is:

- enabled by default after a first-run notice
- classified locally: each install and update event is marked public, local-only, or private
- never transmitted today because no emission backend exists
- immediately opt-outable with `skillctl telemetry disable`

Classification and consent run on your machine, and nothing leaves it.
Private repository identifiers, skill contents, arbitrary diffs, and private
local paths are never part of a telemetry event.

## Troubleshooting

Start here:

```bash
skillctl doctor
skillctl explain <skill>
skillctl path <skill>
skillctl history <skill>
```

Common cases:

- missing skill in one runtime: check `explain` and `path`
- stale generated copy: run `skillctl sync`
- same-name conflict or shadowing: inspect `explain`
- local edits blocking updates: check `doctor` and `update`
- edits kept inside a generated copy: see the preserved-copy warning from `sync`
- every command fails with `not a readable SQLite database`: run
  `skillctl doctor --repair-state --apply`. Plain `skillctl doctor` keeps
  answering and reports the failure as a `state-database-unreadable` issue

Full guide: [docs/troubleshooting.md](./docs/troubleshooting.md)

## Exit Codes

| Code | Meaning |
|---|---|
| `0` | success |
| `1` | operational error |
| `2` | success with warnings |
| `3` | validation or conflict failure |
| `4` | trust gate blocked |
| `5` | interactive input required |

A closed output pipe is not a command failure. `skillctl list | head` exits
with `0` when the reader stops early, instead of reporting a panic status.

## Development

Local checks:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
```

More detail:

- user guide: [docs/user-guide.md](./docs/user-guide.md)
- product direction: [spec.md](./spec.md)
- troubleshooting: [docs/troubleshooting.md](./docs/troubleshooting.md)
- development and toolchain policy: [docs/development.md](./docs/development.md)
