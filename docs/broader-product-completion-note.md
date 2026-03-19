# skillctl: What Still Needs To Happen For Broader Product Completion

This note captures the current state of `skillctl` after a fresh pass over the draft spec, the current implementation, the docs, and the test surface.

The short version:

- `skillctl` looks close to feature-complete for the defined v1 CLI surface.
- It does not yet look broadly product-complete in the larger sense of long-term product maturity, ecosystem breadth, enterprise readiness, and post-v1 expansion.

This note explains the remaining work in detail.

## 1. Current Status

The current implementation appears to satisfy the core v1 lifecycle surface described in the spec:

- workspace bootstrap with `init`
- install from Git, local paths, and archives
- projection materialization with `sync`
- update inspection and next-action planning
- pin, rollback, fork, enable, disable, remove, path, validate, clean
- diagnostics through `doctor` and `explain`
- local history and telemetry consent
- a read-only TUI
- an MCP server with the documented v1 tool set

The codebase, tests, release scripts, docs, and help text all support that reading.

So if the question is "is the v1 command surface here?", the answer is close to yes.

If the question is "is the broader product fully complete and mature enough that only maintenance remains?", the answer is no.

## 2. The Most Important Remaining Work

These are the items that matter most if the goal is to move from "strong v1" to "broader product complete."

### 2.1 Finish The Update Story Beyond Planning

The biggest functional gap is the update workflow.

The spec says `skillctl update [skill]` should:

- check for updates
- show local modifications
- propose the next action
- apply or defer explicitly

The current implementation is intentionally more conservative than that. The CLI help and docs state that `update` reports plans and follow-up actions instead of blindly overwriting local changes.

That is the right safe default, but it still leaves a product gap:

- there is no complete end-to-end "apply this recommended update" workflow
- there is no explicit staged update execution model that takes the planner result and carries it through
- there is no integrated path for "apply", "create overlay", "detach", or "skip" as first-class update outcomes

To close this gap, the product still needs:

- an explicit update-apply flow
- a durable planner-to-executor contract
- safe conflict handling for overlay, drift, and detached states
- clear rollback points when update application fails mid-flight
- UX that keeps the conservative safety model while reducing manual follow-up

Without that, `update` is useful, but the lifecycle is still split between diagnosis and manual execution.

### 2.2 Harden Local State Migration And Repair

The SQLite state store is a major part of the product, and it now carries meaningful user history and lifecycle records.

The current implementation has schema versioning and migrations, but broader product readiness needs more than a happy-path migration.

In practice, we already saw a sign of this: a manual run against an existing developer state database hit a migration rename collision around `*_v1` tables. That suggests the current migration path is not yet robust enough against interrupted, partial, or previously failed upgrades.

What still needs to happen:

- make local state migrations idempotent and recoverable
- handle partially migrated databases without requiring manual SQLite surgery
- add repair or recovery tooling for local state
- add tests for interrupted migration paths, not just clean version hops
- ensure user history remains safe across multiple released versions

This is the clearest gap between "it works in tests" and "it is safe across real user upgrades."

### 2.3 Complete The Telemetry Product, Not Just The Telemetry Policy

The current telemetry implementation is strong on policy and classification:

- first-run notice
- local consent tracking
- public-only classification
- suppression for local and likely private sources
- local history preservation when telemetry is off

What it does not yet represent is a complete telemetry product in the broader sense.

The spec goes beyond local consent and classification. It also describes telemetry as a way to answer product questions such as:

- which public skills are popular
- how often updates are available
- how often local modifications block upgrades
- which doctor failures are most common

It also recommends a backend direction with an ingestion endpoint and queryable append-only event storage.

That broader telemetry layer is still missing. There is no visible remote ingestion pipeline in the current implementation. The current code prepares telemetry reports and classifies whether an event would be eligible for remote emission, but it does not complete the larger system needed for product analytics.

Broader completion here requires:

- a real ingestion path
- event transport and retry behavior
- offline-safe delivery rules
- a queryable backend
- privacy review and retention policy
- operational dashboards for the product team
- configuration and policy controls for different environments

Until that exists, telemetry is best described as locally implemented policy and event shaping, not a finished telemetry product.

### 2.4 Add Full Product-Grade Repair And Recovery Workflows

`doctor`, `explain`, `path`, and `history` are strong inspection tools. They make the system explainable, which is one of the most important design wins in the product.

But broader product completion needs stronger recovery workflows, not just diagnostics.

Today, the tool is better at telling the user what is wrong than at automatically repairing all of it.

The product still needs more first-class repair flows for:

- broken state-store migrations
- stale or partially removed cached imports
- manifest and lockfile divergence
- projection drift recovery where the safe fix is obvious
- partial damage from interrupted old versions or external edits

The key product shift is:

- from "diagnose and recommend next commands"
- to "diagnose, preview, and repair safely"

That would move the system closer to an operations-grade control plane instead of a powerful but still manual lifecycle tool.

## 3. Broader Product Expansion Beyond V1

These areas are not necessarily blockers for calling v1 done, but they are still missing if the goal is the broader finished product suggested by the later phases of the spec.

### 3.1 Expand Scope Beyond `workspace` And `user`

The spec defines `workspace` and `user` as the minimum scopes for v1, and explicitly names future scopes:

- `admin`
- `org`
- `plugin`

Those scopes are not part of the current implementation.

If `skillctl` is meant to become a serious multi-team or enterprise control plane, scope expansion matters because it unlocks:

- centrally managed approved skill catalogs
- org-level read-only baselines
- machine or admin-managed skill roots
- plugin or extension-level sources that are not tied to one repo

This is optional for v1, but still part of the broader product story.

### 3.2 Add Packaging And Export Helpers

The spec explicitly leaves room for package and export helpers and raises `skillctl pack` as an open question.

That is still absent from the current product surface.

Broader completion likely needs:

- a portable package or export format
- a `pack` or `export` workflow
- deterministic archive generation for skill bundles
- a clean bridge between repo-root packaging layouts and installed runtime layouts

Right now the product is strong at consuming skills, but not yet at turning managed skills back into portable distributable artifacts.

### 3.3 Decide Whether Registry Integration Is In Scope

The spec’s ecosystem-expansion phase mentions optional registry integration.

The current implementation is intentionally local-first and source-driven:

- Git
- local path
- local archive

That is coherent for v1, but broader product completion will eventually need a decision on whether `skillctl` remains source-native forever or grows:

- a lightweight registry
- registry metadata lookups
- approved-source catalogs
- curated public install sources

This is a product strategy question as much as an implementation question, but it remains unresolved.

### 3.4 Expand MCP Beyond The V1 Tool Set

The current MCP bridge matches the documented v1 tool set well. That is the right target for now.

But broader product completion should likely mean full machine-usable parity with the CLI, not just v1 parity.

Today, the MCP surface intentionally stops at the listed v1 tools. That leaves a second-tier set of lifecycle features outside the agent-safe machine surface, including areas such as:

- pinning
- forking
- enabling and disabling imports
- path inspection
- cleaning generated state
- telemetry enable and disable

If the long-term goal is "humans and agents use the same system," the product should probably expose the full safe lifecycle surface over MCP, not just the v1 subset.

### 3.5 Keep Adapter Coverage Moving With The Ecosystem

The current adapter registry covers the runtimes called out in the current spec:

- Codex
- Claude Code
- GitHub Copilot
- Gemini CLI
- Amp
- OpenCode

That is enough for v1, but not for a broader long-term product.

Completion in the broader sense means:

- adding more adapters as the ecosystem evolves
- keeping pace with runtime discovery-root changes
- adapting when neutral roots converge or fragment
- handling compatibility metadata and linting for more vendor-specific fields

The product is only as complete as its adapter coverage stays current.

## 4. Security And Trust Need Another Phase

The current implementation does have meaningful trust and risk behavior:

- unreviewed imported skills are surfaced as such
- script-bearing imports can trigger stronger warnings
- symlink mode is guarded with adapter-specific warnings and opt-in semantics

That is solid v1 behavior.

The spec’s broader ecosystem phase, however, explicitly calls out richer security and audit features, and that work is still ahead.

Broader completion here likely needs:

- source provenance improvements
- stronger public-source verification than a small allowlist model
- policy controls for what kinds of sources are allowed
- richer audit reporting for what changed and why
- more explicit trust promotion workflows
- clearer enterprise posture around imported scripts, hooks, and vendor-specific behavior

Today the product warns intelligently. A broader product would also support stronger governance.

## 5. Telemetry, Policy, And Enterprise Readiness Need Clear Decisions

The spec still has open questions that matter for the larger product:

- should `admin` or `org` scope exist as read-only inputs?
- should `skillctl pack` be v1 or v2?
- how much repo-creation help should `update` offer when Git and GitHub CLI are present?
- should telemetry defaults vary by environment, region, or enterprise policy?
- should user-scope neutral roots converge if the ecosystem standardizes further?

Those questions are not implementation bugs. They are unresolved product decisions.

Broader product completion requires answering them, because each one affects:

- CLI shape
- state model
- enterprise adoption
- policy management
- long-term compatibility

Until they are decided, the product remains complete for the current slice, but not fully settled as a broader platform.

## 6. TUI And UX Maturity Still Have Room To Grow

The current TUI is explicitly read-only, and that aligns with the current implementation and docs.

That is fine for v1. It keeps inspection safe and deterministic.

But broader product completion may need a stronger UX layer around guided action:

- previewing recommended update actions
- stepping through repair flows
- promoting overlays or forks from diagnosed drift
- surfacing migration or state-repair options
- helping users resolve same-name conflicts without dropping back to trial-and-error CLI use

This does not necessarily mean building a heavy interactive UI. It means completing the product loop from "inspect state" to "resolve state safely."

## 7. What Is Not Actually Missing

A few things should not be treated as missing just because they are not present:

- a hosted registry is explicitly not required for v1
- a GUI desktop app is explicitly out of scope for v1
- a generalized manager for all harness artifacts is also out of scope for v1
- the current read-only TUI is not itself a failure; it is a deliberate design choice

That distinction matters, because broader product completion should build from the existing product identity, not dilute it into unrelated tool-management scope.

## 8. Recommended Order Of Work

If the goal is to move from "v1 complete" to "broader product complete," the order of operations should probably be:

1. State-store hardening and repair.
2. Finish the update flow beyond planner-only recommendations.
3. Build the telemetry backend and policy model, or explicitly narrow telemetry ambitions.
4. Add stronger repair and recovery workflows on top of `doctor`.
5. Expand MCP toward full CLI parity.
6. Decide and implement the next product-expansion slice:
   `admin` or `org` scope, `pack`, registry support, or richer security and audit.
7. Continue adapter expansion and ecosystem maintenance as a permanent track.

## 9. Bottom Line

`skillctl` is in a strong place as a v1 local-first skill lifecycle tool.

It is not yet fully complete as the broader product described by the full arc of the spec.

The main remaining work is not "add a few more commands." It is:

- finish the lifecycle loop for updates and repair
- harden migrations and long-lived local state
- complete the telemetry system beyond local eligibility logic
- expand the machine and enterprise surface beyond the current v1 slice
- decide which ecosystem-expansion bets are actually part of the product

That is the difference between a strong implementation of the current product slice and a broadly complete product platform.
