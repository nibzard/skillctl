# skillctl

`skillctl` is a local-first, cross-agent skill manager for the open `SKILL.md` ecosystem.

The project is focused on two core problems:

- installing and updating skills across multiple agent runtimes,
- tracking local history and optional telemetry for managed skills.

The current product direction is defined in [spec.md](./spec.md).

At a high level, `skillctl` is intended to:

- use `.agents/` as the default workspace control plane,
- install skills from public Git repos and local sources,
- pin installed skills to exact revisions,
- preserve local changes through overlays and rollback,
- project effective copies into agent-visible roots,
- help humans and agents debug why a skill is missing, stale, or conflicting.

The implementation target is Rust, with test-driven development as a core engineering requirement.

## Install

Install the latest published release with the versioned release installer:

```bash
curl -fsSL https://github.com/nibzard/skillctl/releases/latest/download/skillctl-install.sh | sh
```

To pin a specific release or install into a custom directory:

```bash
curl -fsSL https://github.com/nibzard/skillctl/releases/latest/download/skillctl-install.sh | \
  SKILLCTL_VERSION=v0.1.0 SKILLCTL_INSTALL_DIR="$HOME/.local/bin" sh
```

The release workflow publishes deterministic archives and checksums for:

- Linux `x86_64`
- macOS `x86_64`
- macOS `arm64`
- Windows `x86_64` as a zip artifact for manual download

Tagging `v*` runs [`.github/workflows/release.yml`](./.github/workflows/release.yml), smoke-tests the release binary's first run, packages the archives, and publishes checksums plus the installer asset.

## Quality Gates

CI enforces the same local checks that keep the Rust codebase clean:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
```

The library crate also denies missing public API documentation and broken intra-doc links, so doc coverage regressions fail the docs gate instead of drifting silently.
