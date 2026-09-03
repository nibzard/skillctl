# Note: SkillZip Pro fit for `skillctl`

Source paper: "SkillZip Pro: Execution-Aware Dynamic Compression of
Progressively Loaded Skills for Self-Evolving Agents"
(arXiv 2608.30785, Sep 2026).
Page: https://academy.dair.ai/papers/skillzip-pro-execution-aware-dynamic-compression-of-progressively-loaded-skills-2608.30785

## What the paper does

Agent skills load progressively. A root file loads at activation.
References, scripts, and nested subskills load only when an execution
path needs them. SkillZip Pro compresses such bundles without changing
the agent harness, with two safeguards:

- Cross-file compression: drop content that the root file or a declared
  environment contract already covers.
- Routing preservation: every required file and callable entry stays
  reachable after rewriting.

It reports four modes (one-shot, continual, persistent, transient) and
these results on a production content-moderation skill:

- 38% skill-bundle token reduction with no quality loss.
- 10.4% end-to-end per-run token reduction.
- Negative result: an unprotected 71%-compression configuration lost up
  to 26 accuracy points. The safeguards are the feature, not an option.

## Fit for `skillctl`

`skillctl` already sits between canonical skills (`.agents/skills/`)
and runtime-visible projections. That is the position this paper
targets. The mapping:

| Paper concept | `skillctl` equivalent |
|---|---|
| Transient mode: canonical bundle stays byte-identical, per-run view is compressed | Compress in `src/materialize.rs` at projection time. Never touch the canonical copy. Matches the rule "edit canonical skills or overlays, not generated runtime copies". |
| Continual mode: re-compress after each evolution patch | The `update` flow for pinned skills in `skills-lock.json`. Re-apply compression as a deterministic step. |
| Routing preservation audit | `skillctl doctor`. It already walks projections. Add a check: every path `SKILL.md` references must resolve in the projected tree. |
| Entry contracts (private, public, conditional) | No current equivalent. Overlays or lockfile metadata could store them. |

## Opportunities, in order of value

1. Token accounting in `explain` and `doctor`. Report root-loaded
   tokens versus on-demand tokens per skill. Cheap, safe, and no
   rewrite risk. `skillctl` sees the whole tree; the harness does not.
2. Compression as a planner transform. Model it like overlays:
   recorded, reproducible, keyed in the lockfile. Updates then re-apply
   it without drift.
3. Routing audit gate. If a `compress` command ever lands, the audit
   must land first. The paper's 26-point drop is the warning.

## Cautions

- `skillctl` currently treats skill files as opaque bytes.
  Content-aware compression is a scope change, not a small feature.
- The paper's safeguards depend on entry contracts. The open `SKILL.md`
  ecosystem does not declare them. We would need to infer them or store
  them ourselves.

## Recommendation

- Start with token accounting in `skillctl explain`.
- It creates no rewrite risk.
- It tells us whether compression would pay off for real workspaces
  before any compressor work begins.

References:

- Local project:
  - `README.md`
  - `spec.md`
  - `src/materialize.rs`
  - `src/planner.rs`
  - `src/doctor.rs`
  - `skills-lock.json`
- External:
  - https://academy.dair.ai/papers/skillzip-pro-execution-aware-dynamic-compression-of-progressively-loaded-skills-2608.30785
  - arXiv 2608.30785
