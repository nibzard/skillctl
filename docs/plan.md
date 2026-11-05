# skillctl work plan

Status: Active
Date: 2026-09-01
Source: Multi-perspective project review (JTBD, positioning, product, technical, strategic)

This plan orders the remaining work by risk and by user value. Complete the phases in order. Do not start a later phase before the earlier phase is done, unless a task is independent and marked as such.

## Guiding decisions

These decisions come from the review. They shape every phase below.

1. The durable value of skillctl is the lockfile, the overlay model, and the diagnostics. The projection engine is plumbing. Vendors converge on `.agents/skills` and they build native skill commands. They will not build cross-agent lockfiles, overlays, or diagnostics.
2. The primary wedge is the diagnostic job: "my skill does not load and I do not know why." The strongest under-sold job is team reproducibility: "a teammate clones the repo and gets the exact pinned skills."
3. Decide the product ambition before Phase 4: maintained tool (stop after Phase 3) or long-term product (continue into Phase 4 and beyond). Do not straddle.

## Phase 1 — Restore correctness (do first)

The adapter matrix encodes vendor discovery behavior as of 2026-03-19. The last commit is from 2026-04-09. Five months of runtime churn can make `doctor` confidently wrong, which is worse than absent.

- [ ] Re-verify the compatibility matrix (spec.md Appendix A) against current vendor documentation for Codex, Claude Code, GitHub Copilot, Gemini CLI, Amp, and OpenCode.
- [ ] Update the adapter registry (`src/adapter.rs`) where discovery roots, precedence, or symlink support changed.
- [ ] Update spec.md Appendix A and Appendix B with the new verification date and sources.
- [ ] Add a documented cadence for adapter re-verification (for example: each release, and at least quarterly).
- [ ] Evaluate: move adapter facts from code into a versioned data table so updates do not require logic changes.

Acceptance: `doctor` and the planner agree with current vendor documentation. The spec records the verification date.

## Phase 2 — Close the update loop

`update` plans but never applies. This breaks the core promise of a lifecycle manager. Users adopt the tool to avoid manual file surgery and the tool sends them back to it.

- [ ] Design the planner-to-executor contract: the update plan becomes a durable input to an apply step.
- [ ] Implement `skillctl update <skill> --apply` for the clean case (no local changes).
- [ ] Implement the conflict outcomes as first-class apply options: create overlay, detach to canonical, skip and keep pin.
- [ ] Make apply transactional: manifest, lockfile, store, and projections update together or not at all.
- [ ] Record a rollback point before each apply so a failed apply recovers cleanly.
- [ ] Write failing tests first for each outcome path, including mid-flight failure.
- [ ] Update user guide, troubleshooting guide, README, and MCP `skills_update` to expose apply.

Acceptance: a user can go from "update available" to "update applied" or "overlay created" with one command, and can roll back with one command.

## Phase 3 — Harden local state

The state store is the system of record. A live developer database already hit a `*_v1` table rename collision during migration. This is the one place a user can be hurt permanently.

- [ ] Make all schema migrations idempotent. A re-run of a completed or interrupted migration must succeed.
- [ ] Add tests for interrupted and partially applied migrations, not only clean version hops.
- [ ] Add `skillctl doctor --repair-state` (or equivalent) to detect and repair a damaged or partially migrated database.
- [ ] Back up the database file before each migration and keep the most recent backup.
- [ ] Document the recovery path in docs/troubleshooting.md.

Acceptance: a user upgrades across multiple released versions, with interruptions, and keeps their history without manual SQLite surgery.

## Phase 4 — Resolve telemetry (decide, then act)

Telemetry is on by default, but no ingestion backend exists. Today the tool pays the trust cost and collects nothing. Pick one option:

- Option A (recommended for tool ambition): set the default to off, or make emission a no-op. Keep the consent and classification code. Remove the trust cost.
- Option B (product ambition): build the ingestion endpoint, retry and offline rules, a queryable backend, a retention policy, and a public privacy page. Only then keep the on-by-default setting.

- [ ] Decide Option A or Option B and record the decision in spec.md.
- [ ] Implement the chosen option.
- [ ] Align README, first-run notice, and docs with the actual behavior.

Acceptance: the default behavior, the documentation, and the backend reality all agree.

## Phase 5 — Reposition and distribute

Reframe from "projects skills into runtime roots" to "the lockfile and diagnostics layer for agent skills: pin, customize, and debug skills across every agent, reproducibly."

- [ ] Rewrite the README opening around the team-reproducibility job and the `doctor` wedge. Demote projection to plumbing.
- [ ] Lead the quickstart with the clone-and-sync team flow and the debugging flow.
- [ ] Publish to crates.io (also protects the name).
- [ ] Add a Homebrew tap or formula (the release-runbook already supports formula updates).
- [ ] Review exit code 2 ("success with warnings"): it fails `set -e` scripts. Plan a change for the next breaking window, or document the pattern and add `--strict-warnings`.

Acceptance: a new user understands the lockfile and diagnostics value in the first screen of the README, and can install with `brew` or `cargo install`.

## Phase 6 — Optional expansion (only with product ambition)

Do not start this phase without an explicit decision to run skillctl as a long-term product. Order inside this phase is flexible.

- [ ] Expand MCP to full CLI parity: pin, fork, enable, disable, path, clean, telemetry controls.
- [ ] Repair workflows on top of doctor: preview and apply safe fixes for drift, stale imports, and manifest-lockfile divergence.
- [ ] Decide on registry integration, `skillctl pack`, and `admin`/`org` scopes (spec.md section 28 open questions).
- [ ] Add adapters as the ecosystem grows.

## Explicitly not planned

- A hosted registry, a GUI app, or a general harness-artifact manager. These stay out of scope per spec.md section 3.4.
- A rewrite of any existing subsystem. All phases update the current implementation.

## Review checkpoints

- After Phase 1: is the ecosystem drift small or large? Large drift raises the maintenance cost estimate for the ambition decision.
- After Phase 3: make the tool-versus-product decision before any Phase 4 Option B or Phase 6 work.
