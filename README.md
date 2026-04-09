# skillctl

[![CI](https://img.shields.io/github/actions/workflow/status/nibzard/skillctl/ci.yml?branch=main&label=ci)](https://github.com/nibzard/skillctl/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/nibzard/skillctl?label=release)](https://github.com/nibzard/skillctl/releases/latest)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](./LICENSE)

`skillctl` is a local-first skill manager for the open `SKILL.md` ecosystem.

It gives you one clean workflow for:

- keeping canonical workspace skills in one place
- installing skills from Git, local directories, or archives
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
- local history, diagnostics, and optional public-only telemetry

Important rule:

- edit canonical skills or overlays, not generated runtime copies

## Install

Install the latest release:

```bash
curl -fsSL https://github.com/nibzard/skillctl/releases/latest/download/skillctl-install.sh | sh
```

Install a specific version or custom destination:

```bash
curl -fsSL https://github.com/nibzard/skillctl/releases/latest/download/skillctl-install.sh | \
  SKILLCTL_VERSION=v0.1.0 SKILLCTL_INSTALL_DIR="$HOME/.local/bin" sh
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
skillctl sync
```

Update or pin later:

```bash
skillctl update ai-sdk
skillctl pin ai-sdk main
skillctl rollback ai-sdk <version-or-commit>
```

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

## Core Commands

| Command | What it does |
|---|---|
| `skillctl init` | Create the workspace manifest, lockfile, and default layout |
| `skillctl install <source>` | Detect and install skills from Git, local paths, or archives |
| `skillctl sync` | Rebuild generated runtime projections |
| `skillctl update [skill]` | Check upstream state and recommend a safe next action |
| `skillctl list` | Show managed installs, projections, and current state |
| `skillctl explain <skill>` | Show the active winner, shadowed candidates, and visibility |
| `skillctl path <skill>` | Show canonical, cached, overlay, planned, and projected paths |
| `skillctl doctor` | Diagnose drift, conflicts, shadowing, and trust issues |
| `skillctl history [skill]` | Show installs, updates, rollbacks, overlays, and cleanup events |
| `skillctl override <skill>` | Create or reuse an overlay for a managed import |
| `skillctl fork <skill>` | Copy an imported skill into local canonical ownership |
| `skillctl telemetry status` | Inspect current telemetry settings |
| `skillctl tui` | Open the read-only terminal dashboard |
| `skillctl mcp serve` | Run the MCP server with v1 tool parity |

Every command also supports `--json`.

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
5. Use `override` for small changes and `fork` when you want full ownership.

## Telemetry

Telemetry is:

- enabled by default after a first-run notice
- public-source only
- content-free
- immediately opt-outable with `skillctl telemetry disable`

It never includes private repository identifiers, skill contents, arbitrary diffs, or private local paths.

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
