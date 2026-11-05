# skillctl development plans

Created 2026-11-05 against repository commit `f9fd697`.

## Active roadmap

| Plan | Scope | Priority | Status |
| --- | --- | --- | --- |
| [001: Product and development roadmap](001-product-and-development-roadmap.md) | Reliable local engine, macOS team product, security, vendor distribution, paid company observability and learning | P0 foundations; staged expansion | PROPOSED |

The roadmap is the consolidated planning artifact requested by the maintainer. It includes W01–W15 work packages, dependencies, current-state evidence, verification gates, launch criteria, and commercial decision gates. It is not authorization to execute all phases or deploy a service.

## Execution order

1. Validate customers and desktop/vendor feasibility (W01), alongside a clean verification baseline (W02).
2. Fix migration/concurrency/recovery (W03), then frozen restore (W04) and guarded updates (W05).
3. Specify contracts/policy (W06), build the private service (W07), and implement release review/security (W08).
4. Build the macOS companion (W09), verify vendor publishing (W10), and complete the pilot/release gate (W11).
5. Expand into paid administration (W12), runtime analytics (W13), and champion learning/evaluation (W14) only after the relevant evidence gates pass.
6. Add Windows or advanced deployment options (W15) against demonstrated demand.

Before implementing a work package, create a bounded issue/plan with exact files and build/test commands for any new component. Keep subsequent plan numbers monotonic. Update status with evidence, not merely the presence of implementation code.

## Existing work preserved

`docs/plan.md` was already untracked and remains untouched. The new roadmap records where it extends or changes earlier product assumptions in `spec.md` and the earlier completion note.

## Considered and rejected

- A rewrite of the existing Rust core solely for UI sharing: retain the engine and version its interface.
- A public marketplace, Windows UI, or general harness manager in the first release: defer expansion.
- Treating local projection, package export, publication, invocation, and task success as the same state: require separate evidence.
- Treating scanning, a signature, or a local fork as universal proof of safety: use distinct evidence and digest-bound approval.
- Building all company analytics before proving team distribution: stage the investment behind customer and runtime-evidence gates.
