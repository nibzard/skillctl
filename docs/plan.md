# skillctl work plan

Status: Subordinate engine-history plan; `plans/001-product-and-development-roadmap.md` is the controlling roadmap (maintainer decision 2026-11-07)
Date: 2026-09-01
Reconciled: 2026-11-07
Source: Multi-perspective project review (JTBD, positioning, product, technical, strategic)

This plan orders the remaining work by risk and by user value. Complete the phases in order. Do not start a later phase before the earlier phase is done, unless a task is independent and marked as such.

Reconciliation 2026-11-07: the maintainer selected `plans/001-product-and-development-roadmap.md` as the controlling roadmap. This document is engine history. Each phase heading below names where the work lives today. Phases 1–3 are complete; plans/001 section 15 records the evidence. The Phase 4 and Phase 5 dispositions are recorded in plans/001 section 20. Phase 6 is staged behind the plans/001 work packages and their decision gates.

## Guiding decisions

These decisions come from the review. They shape every phase below.

1. The durable value of skillctl is the lockfile, the overlay model, and the diagnostics. The projection engine is plumbing. Vendors converge on `.agents/skills` and they build native skill commands. They will not build cross-agent lockfiles, overlays, or diagnostics.
2. The primary wedge is the diagnostic job: "my skill does not load and I do not know why." The strongest under-sold job is team reproducibility: "a teammate clones the repo and gets the exact pinned skills."
3. Decide the product ambition before Phase 4: maintained tool (stop after Phase 3) or long-term product (continue into Phase 4 and beyond). Do not straddle.

## Phase 1 — Restore correctness (complete 2026-11-05; plans/001 W02, backlog T049)

The adapter matrix encodes vendor discovery behavior as of 2026-03-19. The last commit is from 2026-04-09. Five months of runtime churn can make `doctor` confidently wrong, which is worse than absent.

- [x] Re-verify the compatibility matrix (spec.md Appendix A) against current vendor documentation for Codex, Claude Code, GitHub Copilot, Gemini CLI, Amp, and OpenCode. Done 2026-11-05; see the verification log in [docs/development.md](./development.md).
- [x] Update the adapter registry (`src/adapter.rs`) where discovery roots, precedence, or symlink support changed. Copilot gained the neutral roots, Amp gained `.claude/skills` and two user roots, and every adapter now records `verified_on` and `documentation_sources`.
- [x] Update spec.md Appendix A and Appendix B with the new verification date and sources. Both appendices record the 2026-11-05 date and cite the development-guide log.
- [x] Add a documented cadence for adapter re-verification (for example: each release, and at least quarterly). The development guide requires a re-verification before each release and at least quarterly.
- [x] Evaluate: move adapter facts from code into a versioned data table so updates do not require logic changes. Evaluated under W02: the facts stay in code, and each adapter carries `verified_on` plus `documentation_sources` metadata instead (plans/001 W02, step 5).

Acceptance: `doctor` and the planner agree with current vendor documentation. The spec records the verification date.

## Phase 2 — Close the update loop (complete 2026-11-05; plans/001 W05, backlog T052)

`update` plans but never applies. This breaks the core promise of a lifecycle manager. Users adopt the tool to avoid manual file surgery and the tool sends them back to it.

- [x] Design the planner-to-executor contract: the update plan becomes a durable input to an apply step. `update --plan` writes a typed plan document and `--plan-file` applies exactly that document.
- [x] Implement `skillctl update <skill> --apply` for the clean case (no local changes).
- [x] Implement the conflict outcomes as first-class apply options: create overlay, detach to canonical, skip and keep pin.
- [x] Make apply transactional: manifest, lockfile, store, and projections update together or not at all.
- [x] Record a rollback point before each apply so a failed apply recovers cleanly.
- [x] Write failing tests first for each outcome path, including mid-flight failure. `tests/cli/update_apply.rs` covers each outcome and interrupted-apply recovery.
- [x] Update user guide, troubleshooting guide, README, and MCP `skills_update` to expose apply. The MCP surface ships `skills_update_apply`.

Evidence: plans/001 section 15, package W05 (backlog task T052).

Acceptance: a user can go from "update available" to "update applied" or "overlay created" with one command, and can roll back with one command.

## Phase 3 — Harden local state (complete 2026-11-05; plans/001 W03, backlog T050)

The state store is the system of record. A live developer database already hit a `*_v1` table rename collision during migration. This is the one place a user can be hurt permanently.

- [x] Make all schema migrations idempotent. A re-run of a completed or interrupted migration must succeed.
- [x] Add tests for interrupted and partially applied migrations, not only clean version hops. `tests/cli/state_recovery.rs` terminates operations at journal boundaries and checks row identity, not only schema versions.
- [x] Add `skillctl doctor --repair-state` (or equivalent) to detect and repair a damaged or partially migrated database.
- [x] Back up the database file before each migration and keep the most recent backup. `src/state/schema.rs` records a migration backup in the journal.
- [x] Document the recovery path in docs/troubleshooting.md.

Evidence: plans/001 section 15, package W03 (backlog task T050).

Acceptance: a user upgrades across multiple released versions, with interruptions, and keeps their history without manual SQLite surgery.

## Phase 4 — Resolve telemetry (open; maintainer decision 2026-11-07)

Decision 2026-11-07: telemetry stays enabled by default, and the claims get corrected. Nothing is transmitted today because no emission backend exists. Backlog task T134 supersedes the Option A and Option B framing below and carries the alignment work.

Telemetry is on by default, but no ingestion backend exists. Today the tool pays the trust cost and collects nothing. Pick one option:

- Option A (recommended for tool ambition): set the default to off, or make emission a no-op. Keep the consent and classification code. Remove the trust cost.
- Option B (product ambition): build the ingestion endpoint, retry and offline rules, a queryable backend, a retention policy, and a public privacy page. Only then keep the on-by-default setting.

- [ ] Decide Option A or Option B and record the decision in spec.md.
- [ ] Implement the chosen option.
- [ ] Align README, first-run notice, and docs with the actual behavior.

Acceptance: the default behavior, the documentation, and the backend reality all agree.

## Phase 5 — Reposition and distribute (distribution decisions recorded 2026-11-07)

Reframe from "projects skills into runtime roots" to "the lockfile and diagnostics layer for agent skills: pin, customize, and debug skills across every agent, reproducibly."

- [ ] Rewrite the README opening around the team-reproducibility job and the `doctor` wedge. Demote projection to plumbing.
- [ ] Lead the quickstart with the clone-and-sync team flow and the debugging flow.
- [x] Publish to crates.io (also protects the name). Rejected 2026-11-07; see plans/001 section 20.
- [x] Add a Homebrew tap or formula (the release-runbook already supports formula updates). Rejected 2026-11-07; the release workflow has no formula step at HEAD. See plans/001 section 20.
- [x] Review exit code 2 ("success with warnings"): it fails `set -e` scripts. Plan a change for the next breaking window, or document the pattern and add `--strict-warnings`. Reviewed 2026-11-07 and kept as documented; see plans/001 section 20.

Acceptance: a new user understands the lockfile and diagnostics value in the first screen of the README, and can install with `brew` or `cargo install`. The 2026-11-07 decisions rejected both install routes; the curl installer and the GitHub release archives remain the install path, so only the README part of this acceptance still applies.

## Phase 6 — Optional expansion (staged behind the plans/001 work packages)

Do not start this phase without an explicit decision to run skillctl as a long-term product. Order inside this phase is flexible. plans/001 stages this work behind W06–W14 and their commercial decision gates (plans/001 sections 15–19).

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
