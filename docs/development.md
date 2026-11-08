# Development guide

Status: Active
Last verified: 2026-11-05

This guide records the supported Rust toolchain policy, the verification gates,
and the adapter verification log. It is the entry point for anyone changing the
engine.

## Supported Rust toolchain policy

- `skillctl` targets the **current stable Rust toolchain**. CI (`.github/workflows/ci.yml`)
  builds, lints, tests, and documents on the `stable` channel through
  `dtolnay/rust-toolchain@stable`.
- The crate uses Rust edition 2024. Building requires a stable toolchain new
  enough for edition 2024 (stable since 2025-02-20).
- The project deliberately ships **no `rust-toolchain.toml` pin**. Fixing code
  is preferred over pinning a toolchain when a new stable release changes lint
  behavior. If a stable upgrade requires larger changes, keep the smallest fix
  that restores the gates and record it in this file.
- There is **no declared MSRV** while the project is `0.x`. Treat the CI stable
  channel as the compatibility baseline.
- Toolchain upgrade policy: when a new stable release lands, run the gates
  below locally before pushing. `cargo clippy` warnings introduced by a newer
  stable are defects to fix, not lint levels to relax.

## Verification gates

Run all of these before proposing a change. They mirror CI.

```bash
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
```

Focused suites used by the roadmap (plans/001 section 21):

```bash
cargo test --locked --test cli
cargo test --locked --test mcp
cargo test --locked --test integration_matrix
cargo test --locked --test release
```

`cargo build --release --locked` is only a gate when producing or testing a
release.

## Clippy baseline evidence

The repository review recorded in `plans/001` section 4 found four Clippy
diagnostics on the then-current stable toolchain. Commit `81233dc`
(`fix(ci): satisfy clippy on the current stable toolchain (#1)`) fixed that
baseline before work package W02 started.

Evidence at HEAD of this change set, on `rustc 1.98.1 (48a229cea 2026-09-01)`:

```text
$ cargo clippy --all-targets --all-features --locked -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s)
```

The `map_unwrap_or`, `manual_let_else`, `needless_raw_string_hashes`,
`redundant_clone`, and `redundant_closure_for_method_calls` denies in
`Cargo.toml` `[lints.clippy]` keep the historically fragile categories at
error level so regressions fail locally, not only in CI.

## Adapter verification log

`src/adapter.rs` records, per runtime, the date the discovery facts were last
verified (`verified_on`) and the vendor documentation pages that back them
(`documentation_sources`). `skillctl --json doctor` surfaces the same records
in the response payload under `data.adapters`.

Rules:

- Re-verify every adapter before a release and at least quarterly.
- Never plan projections into undocumented paths. If a vendor drops a path,
  remove it and record the change here.
- Update `VERIFIED_ON`, the per-adapter source list, and the
  `DETECTION_ROOTS` cross-check in `src/source.rs` together. The debug
  assertion in `compatibility_registry()` fails the test suite when the two
  tables drift apart.

### 2026-11-05 verification

All six initial adapters were re-checked against current vendor documentation.

| Runtime | Verified workspace roots | Verified user roots | Change |
| --- | --- | --- | --- |
| Codex | `.agents/skills` (scanned from CWD up to the repo root) | `~/.agents/skills` | None |
| Claude Code | `.claude/skills` | `~/.claude/skills` | None |
| GitHub Copilot | `.github/skills`, `.claude/skills`, `.agents/skills` | `~/.copilot/skills`, `~/.agents/skills` | Added the two neutral roots; removed `~/.claude/skills` because the current pages no longer list it for personal scope |
| Gemini CLI | `.gemini/skills`, `.agents/skills` | `~/.gemini/skills`, `~/.agents/skills` | None |
| Amp | `.agents/skills`, `.claude/skills` | `~/.config/agents/skills`, `~/.config/amp/skills`, `~/.agents/skills`, `~/.claude/skills` | Added `.claude/skills` (workspace), `~/.agents/skills`, and `~/.claude/skills` (user) |
| OpenCode | `.opencode/skills`, `.claude/skills`, `.agents/skills` | `~/.config/opencode/skills`, `~/.claude/skills`, `~/.agents/skills` | None |

Sources checked on 2026-11-05:

- Codex: <https://developers.openai.com/codex/skills> (redirects to
  <https://learn.chatgpt.com/docs/build-skills>)
- Claude Code: <https://code.claude.com/docs/en/skills>
- GitHub Copilot:
  <https://docs.github.com/en/copilot/concepts/agents/about-agent-skills> and
  <https://docs.github.com/en/copilot/how-tos/use-copilot-agents/coding-agent/create-skills>
- Gemini CLI: <https://geminicli.com/docs/cli/skills/>
- Amp: <https://ampcode.com/docs/customize/skills>
- OpenCode: <https://opencode.ai/docs/skills>

Planning consequences of this verification:

- Claude Code + GitHub Copilot in user scope no longer share `~/.claude/skills`.
  Copilot plans `~/.copilot/skills`, so the pair now uses two documented roots.
- Codex + GitHub Copilot in user scope can now share `~/.agents/skills`.
- Amp joins the shared `~/.agents/skills` neutral root in multi-target user
  plans instead of creating a separate `~/.config/agents/skills` root.
- Existing installations that used a removed root are reported by `doctor`
  as `wrong-precedence-root`; `skillctl sync` regenerates them in the planned
  roots.

Amp's `~/.claude/plugins/cache/` directory is a plugin-managed cache, not an
authoring or projection root, so it is intentionally excluded from the planner
tables.

## Notes

- `spec.md` Appendix A, Appendix B, and the section 15.3 planner tables now
  record this log's 2026-11-05 matrix (reconciled 2026-11-06). Update them
  together with `VERIFIED_ON` at the next verification pass.
