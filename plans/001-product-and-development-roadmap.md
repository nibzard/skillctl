# skillctl: product strategy and development roadmap

Status: Proposed roadmap; implementation has not started under this plan
Authoring date: 2026-11-05
Repository baseline: `f9fd697` (`0.1.0`, Rust edition 2024)
Initial desktop platform: macOS
Long-term direction: a paid company service for approved skill distribution, security, observability, evaluation, and organizational learning

Reading guide:

- [Product thesis and five jobs](#2-product-thesis-and-positioning)
- [Current repository and gaps](#4-current-repository-state-and-reusable-assets)
- [macOS v1 scope](#5-scope-and-definition-of-macos-v1)
- [Architecture](#7-system-architecture)
- [Versioning guarantees](#9-versioning-and-reproducibility-contract)
- [Security and approvals](#10-security-model-and-approval-lifecycle)
- [ChatGPT, Claude, and other destination integrations](#14-destination-integration-strategy)
- [W01–W15 implementation packages](#15-execution-roadmap-and-dependencies)
- [Paid company service](#16-future-paid-company-service)
- [Observability and analytics](#17-observability-and-analytics-specification)
- [Champion-to-team learning](#18-champion-to-team-learning-system)
- [Launch checklist](#22-macos-v1-launch-checklist)
- [Immediate next actions](#immediate-implementation-order)

## 1. Purpose and how to use this document

This document consolidates the product discussion, repository review, and current vendor documentation into one development plan. It is self-contained: an implementer should not need the conversation to understand the intended product, boundaries, priorities, or acceptance criteria.

This is a program roadmap, not permission to implement every phase immediately. Work packages define bounded deliverables and verification requirements. Before each package starts, turn its scope into a small implementation issue or plan with exact changed files and commands for any new component. Do not invent backend or desktop build commands before those projects exist.

The existing `docs/plan.md` was untracked when this document was written and remains untouched. It contains useful earlier priorities. This document is the proposed controlling roadmap for the expanded team-product direction, subject to maintainer acceptance. `spec.md` and `docs/broader-product-completion-note.md` describe an earlier product slice; explicit differences are listed below rather than silently treating those documents as current commitments.

### Decisions carried forward from the discussion

- Support five jobs: individual cross-agent convenience; versioning and updates; team interchange; organizational observability and learning; protection against compromised skills.
- Accept GitHub links, ZIP archives, and local folders as import sources.
- Preserve the existing Rust engine and CLI as the foundation.
- Add a small native desktop companion, starting on macOS; Windows is a later expansion.
- Add team sign-in, a private shared library, immutable stored artifacts, and approval workflows.
- Use S3-compatible object storage for artifacts, with separate metadata and authorization services.
- Integrate with supported ChatGPT and Claude distribution mechanisms rather than assuming local file copies install account-managed skills.
- Preserve a useful local workflow without requiring a hosted account.
- Develop a paid company service after proving distribution and security, including usage analysis, outcome evaluation, and learning from champion users.

### Proposed choices, not yet externally validated

- Start with mixed-agent engineering teams that share private skills across repositories. This is the initial customer hypothesis, not established demand.
- Use SwiftUI/AppKit for the first macOS UI and a versioned JSON interface to a bundled Rust executable.
- Use a modular backend, managed PostgreSQL, an identity provider, object storage, and a worker queue. Select vendors after a short architecture spike; no hosting provider is mandated here.
- Offer a free local tool and paid team management. Pricing and commercial packaging require customer validation.
- Start the paid service with inventory, compliance, and release management; add runtime analytics and outcome evaluation incrementally.

## 2. Product thesis and positioning

**Positioning:** Distribute approved skills across coding agents, reproduce exact versions, and trace their deployment and use.

The product's durable value is the relationship between approved source content, effective local customizations, destination-specific packages, installed state, runtime observations, and measured outcomes. Installation convenience is necessary, but it is not sufficient differentiation.

The product must answer:

1. What exactly is this skill, who owns it, and where did it come from?
2. Which exact bundle was reviewed and approved?
3. Can a new machine reconstruct it without an existing cache?
4. What differs between upstream, a team release, and a user's customization?
5. Where was that release deployed, and how recently was this verified?
6. Where was it invoked, where is evidence missing, and did it help?
7. If a release is compromised, which installations need action?

Do not position the product as a guarantee that arbitrary skills behave identically in every agent. Skills may depend on tools, permissions, metadata, runtime features, credentials, and system packages. Portability means supported installation plus explicit compatibility information, with behavioral claims backed by evaluations.

### The five jobs

| Job | User outcome | Product response | Initial importance |
| --- | --- | --- | --- |
| Individual convenience | Install once and use supported agents without managing directories | Simple import, destination detection, install, explanation | Entry experience |
| Versioning and maintenance | Reconstruct exact content and safely adopt changes | Immutable releases, lockfiles, frozen restore, reviewed updates, rollback | Foundation |
| Team interchange | Share owned, approved workflows across projects and teams | Private library, ownership, access, releases, assignments, publishing integrations | Initial product |
| Observability and learning | Know what is installed, used, current, and effective | Inventory, compliance, runtime evidence, evaluations, champion-to-team workflow | Paid expansion |
| Security | Reduce compromise risk during sharing and updates | Digest-bound approval, integrity, policy, provenance, revocation, runtime integrations | Foundation |

### Competitive reality

As checked on 2026-11-05:

- Vercel's `skills` CLI documents cross-agent installation, private sources, ref selectors, updates, and experimental frozen-lockfile restore and package synchronization.
- Claude documents organization libraries, review of fixed submitted versions, update approvals, scanning on eligible plans, and Enterprise plugin APIs with version selection and access controls.
- OpenAI documents local skills separately from workspace-managed skills/plugins and supports GitHub-backed marketplace import and synchronization.
- OpenCode documents compatible roots, additional sources, precedence, and plugin event interfaces.
- Tessl documents private team publishing, versioning, security findings, and skill evaluations.

These are documented capabilities, not results of an independent end-to-end product benchmark. Recheck them before shipping integrations or making competitive claims. Native vendors are already building team management; the opportunity is a consistent cross-vendor workflow, not an assumption that vendors will ignore the problem.

## 3. Initial customer and product validation

### Initial customer hypothesis

A team of approximately 5–30 developers uses at least two agent families, has at least ten shared or private skills, and needs different repositories to adopt changes at different times. A champion currently maintains informal instructions, copied folders, internal Git repositories, or several vendor libraries.

The initial buyer hypothesis is an engineering lead, developer-experience lead, or security-conscious platform team. Nontechnical departments are a later hypothesis unless early interviews show substantially stronger demand and supported desktop publishing is sufficient for them.

### Validate before expanding infrastructure

- Interview at least five candidate teams; document their actual current process and the tools they already use.
- Ask for a concrete recent failed onboarding, unsafe update, lost customization, or duplicated sharing workflow.
- Observe one skill moving from author to reviewer to at least two consumers.
- Recruit at least three design-partner teams with genuinely mixed-agent usage.
- Test whether Git plus native vendor libraries already solves their problem adequately.
- Ask who would buy, which procurement constraints matter, and what evidence they need before trusting a local installer.
- Record whether a desktop companion is useful or whether the team primarily wants CI and vendor workspace publishing.

Proposed validation gate: continue the hosted product after at least three teams complete the pilot workflow, two ask to continue using it, and at least one agrees to a concrete paid pilot discussion. These thresholds are working decision rules, not market evidence.

If teams are satisfied with a single vendor's management, do not build a duplicate library merely to complete the roadmap. Narrow to local diagnostics, interoperability, security inventory, or another demonstrated gap.

## 4. Current repository state and reusable assets

### Verified baseline from the review

The repository is a Rust CLI/library with typed domain errors, Clap commands, SQLite local state, YAML manifest and lockfile, source import, resolver, overlay, projection, diagnostics, history, telemetry reporting, MCP, and a read-only TUI. It has meaningful unit and integration tests, including returned-error rollback failpoints.

Checks run during the review:

| Check | Result at baseline |
| --- | --- |
| `cargo test --locked` | Passed |
| `cargo fmt --check` | Passed |
| `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps` | Passed |
| `cargo clippy --all-targets --all-features --locked -- -D warnings` | Failed with four diagnostics |

The Clippy failures included three `map_unwrap_or` violations in `src/lifecycle.rs` and `src/source.rs`, plus a diagnostic at the `thiserror` conversion in `src/error.rs`. Inspect the generated-code/toolchain interaction before selecting the smallest fix for the latter. Do not weaken project-wide lint policy just to hide the baseline failure.

### Domain map

| Existing file/module | Responsibility | Planned use |
| --- | --- | --- |
| `src/source.rs` | Source normalization, acquisition, import cache | Import and exact-revision restoration |
| `src/lockfile.rs`, `src/manifest.rs` | Declared state and pinned metadata | Portable reproducibility contract |
| `src/resolver.rs`, `src/overlay.rs` | Effective bundle and local customization | Bundle construction and reviewed customization |
| `src/planner.rs`, `src/history.rs` | Update checking, pinning, rollback, history | Shared update plan and apply engine |
| `src/materialize.rs`, `src/lifecycle.rs` | Filesystem projection and rollback | Safe local deployment and recovery |
| `src/state.rs`, `src/state/schema.rs`, `src/state/codec.rs` | Persistent local state | Migration, journaling, installations, evidence |
| `src/doctor.rs`, `src/adapter.rs` | Diagnostics and runtime assumptions | Observed state and versioned compatibility |
| `src/trust.rs` | Current trust warnings | Replace implicit organizational trust with explicit approvals |
| `src/runtime.rs`, `src/response.rs`, `src/mcp.rs` | Shared command execution and presentation | Stable machine interface for CLI, app, and integrations |
| `src/telemetry.rs` | Public-source install/update event reports | Keep distinct from tenant-owned product observability |
| `tests/cli/*`, `tests/integration_matrix.rs` | Integration behavior | Regression and contract tests |

### Findings that must shape implementation

1. **Fresh-checkout restoration is missing.** `src/materialize.rs:88` enters resolution without acquiring missing locked imports. `src/resolver.rs:474` requires the local stored import directory. `src/source.rs:1113` namespaces workspace caches by a workspace key. A committed lockfile therefore cannot currently reconstruct a new or moved checkout.
2. **Migration completion is not atomic.** `src/state/schema.rs:83` creates the new schema before copying legacy rows; schema SQL at approximately line 407 advances the version before those copies finish. Opening a database does not wrap this migration in one SQLite transaction. A partial migration can be mistaken for a complete current schema.
3. **Rollback can overwrite concurrent state.** `src/lifecycle.rs:57` snapshots the entire shared database and rollback restores files without a process-wide coordination boundary. Another command's successful writes could be reverted. This scenario was identified by inspection, not a concurrency reproduction.
4. **Successful destructive writes do not consistently preserve edits.** `src/materialize.rs:920` deletes prior generated directories before rebuilding. Update checking detects projection edits, but sync, install, or pin can invoke regeneration under a different preservation policy.
5. **Diagnostic state is overstated.** `src/doctor.rs:1718` derives `visible` from winner selection rather than checking that the destination exists. Conflict responses use successful payloads with failure exit overrides; MCP derives `isError` from `ok`, so interfaces disagree.
6. **Compatibility data has drifted.** `src/adapter.rs:229` omits Copilot neutral roots documented today. Recheck every supported destination rather than applying a one-off patch and assuming the rest are current.
7. **Update application remains unfinished.** `src/planner.rs:105` builds and reports update plans. `src/history.rs` already contains mutation paths for pin/rollback; reuse a common engine instead of introducing another independent writer.
8. **Current trust is not organizational approval.** `src/trust.rs` classifies canonical, detached, and forked content as locally trusted. That is ownership semantics, not evidence of organizational review or safety. A fork must not bypass team approval.
9. **Current telemetry is not usage analytics.** `src/telemetry.rs` constructs install/update event reports and sets `emitted` based on eligibility. It does not establish runtime invocation or successful completion.

### Load-bearing excerpts for drift checks

`src/resolver.rs`, current import resolution:

```rust
let stored_source_root = stored_import_root(
    match import.scope {
        ManifestScope::Workspace => crate::state::ManagedScope::Workspace,
        ManifestScope::User => crate::state::ManagedScope::User,
    },
    &request.working_directory,
    &import.id,
)?;
ensure_directory(&stored_source_root, "inspect stored import root", "directory")?;
```

`src/lifecycle.rs`, current returned-error transaction boundary:

```rust
let mut transaction = LifecycleTransaction::new(operation)?;
match work(&mut transaction) {
    Ok(result) => Ok(result),
    Err(error) => match transaction.rollback() {
        Ok(()) => Err(error),
        Err(rollback_error) => Err(rollback_error),
    },
}
```

`src/materialize.rs`, current generated-directory replacement:

```rust
Ok(_) if is_skillctl_generated_directory(target_dir)? => {
    fs::remove_dir_all(target_dir) /* error mapping omitted */ ?;
    fs::create_dir_all(target_dir) /* error mapping omitted */
}
```

These excerpts intentionally omit surrounding implementation; confirm current symbols and full control flow before editing. Never use line numbers alone as proof that the baseline still matches.

## 5. Scope and definition of macOS v1

### In scope

- A reliable standalone CLI with frozen restore, guarded updates, history, and useful diagnostics.
- A signed and notarized macOS application with a menu bar entry and a normal library/review window.
- Browser-based team sign-in, a private library, and basic organization roles.
- Imports from GitHub, ZIP, and selected local folders, including folders containing multiple skills.
- Immutable bundle storage, explicit approval of releases, and audit history.
- Local installation into verified supported agent roots, with destination compatibility information.
- Package export and guided handoff for account-managed desktop surfaces.
- One end-to-end verified organization publishing integration, chosen by design-partner demand and account eligibility.
- Installation receipts and last-verified inventory; no invented usage claims.
- Explicit reviewed updates and recoverable rollback.

### Out of scope for v1

- Windows desktop UI, mobile clients, or a public marketplace.
- General management of every agent hook, rule, subagent, and MCP configuration.
- A new skill authoring format or agent runtime.
- Guaranteed behavioral equivalence across models or agents.
- A universal security scanner that certifies a skill safe.
- Continuous employee surveillance, individual leaderboards, or inferred productivity scores.
- Automatic distribution of an author's private work without explicit sharing.
- Silent editing of vendor app databases, undocumented caches, or UI automation as a production integration.
- Automatic execution of imported scripts during import, inspection, preview, or indexing.
- Dependence on a pre-existing local cache or on ambient developer credentials on the server.

### v1 end-to-end demonstration

An author imports a GitHub link or ZIP, selects the intended skill, and sees the files, provenance, dependencies, and compatibility. A reviewer approves an immutable release. Two teammates sign in on separate Macs; one installs into Codex and another into Claude Code or OpenCode. A fresh checkout on an empty cache restores the same declared content. A changed upstream release appears as a reviewable update. Direct local edits cause a clear block or preservation choice. The team can roll back, and the app accurately distinguishes local installation from vendor-workspace publication and runtime activation.

For ChatGPT and Claude Desktop, demonstrate a supported publishing or guided upload path using the same approved source. If a vendor rebuilds or transforms the package, record that transformation and do not claim byte-level equivalence without verification.

## 6. Primary user journeys

### A. Individual import and installation

1. Open the app and choose “Add skill.” Local use is available without team sign-in.
2. Paste a GitHub URL, drop a ZIP, or select a folder.
3. Acquire into quarantine with no imported code execution.
4. Show discovered skills and ask which to import when the source contains several.
5. Show origin, resolved revision or snapshot, file changes, compatibility, dependencies, and risk findings.
6. Choose destinations and project or user scope.
7. Apply through the shared local engine; verify installed files against the expected destination artifact.
8. Show installation status and next steps for destinations requiring separate vendor authorization or upload.

### B. Share with a team

1. Sign in and choose the correct organization.
2. Import a source or select an existing local candidate to publish.
3. Choose a stable skill identity, owner, audience, and release notes.
4. Explicitly confirm that the uploader is authorized to share the content; preserve applicable license and attribution.
5. Submit the immutable candidate for review.
6. Reviewer inspects files, diff, dependencies, automated findings, and evaluation evidence where available.
7. Approve the exact release digest under a recorded policy version.
8. Publish to the team library and supported destination integrations.
9. Recipients install or receive the release according to assignment and supported enforcement.

### C. Update without losing customization

1. Check the configured source/channel without mutating installed files.
2. Resolve a candidate revision and construct a complete change report.
3. Include upstream changes, new dependencies, permissions, scripts, and destination package changes.
4. Compare current effective content with the prior installed snapshot.
5. If dirty, offer preserve-as-overlay, publish a fork/candidate, or explicit discard after durable backup.
6. Require approval for changed effective content where organizational policy applies.
7. Apply the approved plan only if its preconditions still match; otherwise replan.
8. Verify the installation and retain a rollback reference.

### D. Incident and revocation

1. An authorized security/release owner revokes a release or effective digest, with a reason.
2. The service identifies known destination deployments and installation receipts.
3. Online managed clients refresh policy, block future installation, and remediate according to policy and destination capability.
4. Offline clients remain “unknown/offline” until they acknowledge; do not mark them remediated.
5. Vendor integrations request disablement or replacement where supported and record the response.
6. Show outstanding installations, acknowledgments, and limitations for running sessions or external copies.

## 7. System architecture

### Components

1. **Rust local engine:** parsing, resolution, bundle hashing, lockfile validation, planning, drift detection, safe materialization, local history, and machine-readable results.
2. **macOS companion:** sign-in, organization/library selection, import and review UI, destination selection, background inventory, notifications, and local engine orchestration.
3. **Hosted control service:** identity linkage, tenancy, metadata, ownership, approvals, policy, release channels, publishing orchestration, and audit records.
4. **Artifact store:** private immutable source/effective/destination bundles in S3-compatible storage.
5. **Workers:** source acquisition, archive validation, static checks, package generation, scanning integrations, and vendor publishing. Later, evaluations run in a separate execution environment.
6. **Vendor adapters:** local filesystem adapters and remote publishing/inventory adapters with explicit capabilities.
7. **Observability pipeline, later:** tenant-owned events, deduplication, aggregation, coverage, dashboards, and outcome analysis.

### Architectural choices

- Keep domain logic in Rust and the initial desktop UI thin. Use a versioned JSON protocol over subprocess stdin/stdout. Do not let the app rewrite manifests, projections, or databases independently.
- Bundle a known compatible executable with the app. A separately installed CLI may coexist, but both must use compatible state schemas and shared locking. Refuse unsupported protocol/schema combinations before mutation.
- Use SwiftUI for the main UI and AppKit where needed for the menu bar and lifecycle behavior. Confirm sandbox/file-access requirements in the desktop spike before choosing distribution entitlements.
- Start the backend as a modular service plus worker process, not a collection of microservices. Separate import/scanning jobs from request handling and from any evaluation execution.
- Use managed PostgreSQL for server metadata. Local SQLite remains a cache and local ledger, not the authoritative source for organization approvals.
- Use an established identity provider supporting browser-based native login. Native clients use authorization code flow with PKCE; a CLI can use a supported device authorization flow. Select the provider during the backend spike.
- Store refresh credentials in macOS Keychain; keep tenant administrator integration credentials encrypted server-side. Never distribute vendor administrator keys to member Macs.
- Do not require a resident privileged helper for v1. Run as the signed-in user and request only necessary filesystem access.
- The desktop app should use a background task while running. Do not promise continuous enforcement when the app is quit; mandatory launch agents or device-management integrations are a later explicit decision.

### Object storage responsibilities

- Store immutable artifacts under opaque tenant-scoped keys, with digests in metadata.
- Keep quarantine uploads separate from approved distributable artifacts.
- A signed upload URL authorizes only a bounded upload to one object. Finalization independently verifies size, type, digest, organization ownership, and processing result.
- Issue short-lived authorized download URLs after checking tenant membership and release policy.
- Clients verify digest after download; storage transport success is not integrity verification.
- Do not overwrite an object to change an existing release. New bytes require a new artifact and release identity.
- Do not expose cross-tenant content existence through global deduplication or digest lookup. Defer cross-tenant deduplication.
- Keep retention, deletion, legal holds, and rollback retention explicit. Object versioning is a storage aid, not the release/version model.

### Source acquisition

- Client-local folder imports are uploaded only through an explicit publish action.
- For private GitHub repositories, prefer a GitHub App with repository-scoped access for hosted imports; local-only imports can use the user's existing Git credentials.
- Server fetchers accept supported hosts/protocols with redirect and network-destination restrictions. They must not become an arbitrary internal-network fetch service.
- Disable Git hooks, implicit credential prompting, and automatic submodule/LFS processing unless implemented as explicit supported dependencies. Surface unsupported content rather than silently omitting it.
- Import and inspection do not execute skill scripts or package installation hooks.

## 8. Domain model and source-of-truth rules

### Identities and records

| Entity | Required concepts | Invariant |
| --- | --- | --- |
| Organization | ID, name, policy, retention settings | Every private server resource belongs to exactly one tenant |
| Membership | Organization, user, role, status | Authorization is evaluated server-side on every operation |
| Team/group | Organization, members, external mapping | A vendor group is a mapping, not automatically the same identity |
| Skill | Stable ID, namespace, display name, owner, description | Names are labels; renaming does not change identity |
| Source | Type, canonical locator, subpath, credential reference | Credentials never appear in public provenance or lockfiles |
| Candidate | Source snapshot, digest, author, scan state | Candidate content is immutable once submitted |
| Bundle | Canonical file inventory, content digest, format version | Digest identifies complete packaged content and relevant file attributes |
| Release | Skill ID, version label, bundle digest, release notes | Published release content cannot change in place |
| Effective variant | Base release, overlay digest, effective digest, owner | Customization produces distinct content and approval requirements |
| Destination artifact | Source/effective digest, adapter/version, package digest | Transformation is recorded; destination bytes have their own identity |
| Dependency declaration | Tools, packages, MCP/plugin requirements, constraints | Declared dependencies are distinguished from discovered hints |
| Scan result | Digest, scanner/version, policy, timestamp, findings | Scans are evidence, not a safety certificate |
| Approval | Digest, reviewer, audience, policy version, decision, time | Approval cannot silently transfer to changed content |
| Channel | Skill/variant, audience, approved release pointer | Promotion changes a pointer, not historical content |
| Assignment | Organization/group/project, release/channel, required/optional | Required enforcement depends on destination capabilities |
| Integration | Tenant, vendor, account/workspace ID, scopes, health | Integrations never inherit membership from another tenant |
| Publication | Destination, package ID/digest, remote version ID, state | “Published” is separate from “installed by member” |
| Device | Enrollment ID, user/organization, platform, last seen | Device reports are claims with explicit freshness |
| Installation | Device/project/destination, desired and observed digests | Desired state and observed state can disagree |
| Operation | ID, actor, preconditions, plan digest, state, result | Retries are idempotent and stale plans cannot apply |
| Audit event | Actor, action, tenant, object, before/after references, time | Append-only through normal product APIs |
| Runtime observation | Session, release/variant, event type, provenance, coverage | Missing evidence is not fabricated as non-use |
| Evaluation run | Dataset version, model/agent/tool versions, results | Outcome claims are tied to a reproducible evaluation setup |

### Authority boundaries

- Repository manifest and lockfile define that repository's desired dependency state.
- Organization policy and approvals define what managed distribution is permitted.
- Stored immutable bundles define exact released content.
- Local installation state records observations and local operation history.
- Vendor platforms define their own installation, ownership, authorization, and runtime behavior.
- A vendor publication acknowledgment does not prove that every user has installed or invoked the release.
- A local “trusted” label must not imply organization approval. Separate ownership, review, authenticity, scan findings, and policy eligibility.
- Deleting an organization membership revokes service access; it does not remotely erase every previously downloaded file. Retention and enforcement limits must be explicit.

## 9. Versioning and reproducibility contract

### Identity layers

Use separate fields for:

1. Human release label, such as `1.3.0`.
2. Immutable upstream Git commit, when applicable.
3. Source bundle digest.
4. Overlay digest, if any.
5. Effective content digest.
6. Destination package digest and adapter version.
7. Evaluation/test configuration identity, where behavior is being measured.

A mutable Git tag or branch is a request selector, not the installed version. Resolve it to an immutable revision before approval and lockfile creation. A Git commit plus subpath identifies source content; a bundle digest identifies the retained artifact delivered by the product.

### Canonical bundle format

Define and version a canonical file manifest before relying on hashes for approvals:

- Paths are safe normalized relative paths using a documented encoding and separator convention.
- Inventory entries are sorted deterministically and include file digest, size, and executable status where relevant.
- Include all distributed instructions, references, assets, scripts, and declared configuration; do not hash only `SKILL.md`.
- Exclude transport timestamps and generated local metadata from the source identity.
- Do not silently normalize line endings, Unicode, or content after approval. If transformation is required, produce a distinct destination artifact.
- Reject unsafe path traversal, symlinks, hard links, device files, duplicate normalized paths, and case/Unicode collisions under supported filesystem behavior in v1.
- Bound total size, compressed expansion, file count, path depth, individual file size, fetch duration, and redirects. Choose defaults in the import contract and test them.
- Reject unsupported or ambiguous input instead of silently dropping files.
- Reproducible archives use deterministic ordering and transport metadata; archive-byte digests and canonical content digests remain distinct.

### Portable lockfile

The portable lock records stable skill identity, source provenance without credentials, immutable revision or stored artifact reference, expected digests, overlay identity, and relevant adapter/package constraints. Machine-specific cache paths, access tokens, device IDs, and local database schema internals are not dependency identity.

Migrate the current lockfile deliberately. Preserve old supported locks through an explicit conversion or compatibility loader, with fixtures and documented behavior. Decide whether server-generated destination packages are pinned directly or reproducibly rebuilt using a pinned adapter; do not accept “whatever the newest adapter emits.”

For ZIP/folder imports, create a retained immutable snapshot. Upstream checking is unavailable unless an explicit source association is added. This restriction must be visible in the UI.

### Required operations

Proposed public command shapes below are design targets, not commands implemented today:

```text
skillctl restore --frozen
skillctl verify
skillctl update <skill> --check
skillctl update <skill> --apply
skillctl update <skill> --plan <file>
skillctl apply <plan-file>
skillctl rollback <skill> <release-or-digest>
```

- `restore --frozen` reconstructs from an empty cache, verifies content, and does not change the lockfile or select newer revisions.
- `verify` inspects state without repairing it; report integrity, drift, missing content, policy eligibility, and freshness separately.
- Update checking does not overwrite local skills or advance approved channels.
- An apply plan records expected current state, proposed state, policy/approval references, affected destinations, and required user choices.
- Apply checks plan freshness immediately before mutation. Treat a plan file as untrusted input: recompute paths and authority; never execute arbitrary file operations supplied by the document.
- Rollback is subject to current policy. A revoked release is not automatically eligible as a rollback target.
- Offline restoration succeeds only from verified cached artifacts and acceptable cached policy. It never pretends to have checked upstream.

### Limits of the guarantee

Guarantee exact supplied content and verified local materialization under stated conditions. Do not guarantee identical model output, future upstream availability without retention, absence of malicious intent, or prevention of unmanaged copies. A skill that fetches external scripts or unpinned dependencies at runtime escapes a bytes-only guarantee; expose this limitation and let policy reject or require explicit approval of such behavior.

## 10. Security model and approval lifecycle

### Threats in scope

- Compromised upstream repositories, maintainers, releases, or accounts.
- Malicious instructions without executable scripts.
- Malicious archives, unsafe paths, excessive expansion, or parser/resource abuse.
- Unauthorized cross-tenant reads, writes, approvals, and artifact downloads.
- Review/apply races in which approved content differs from installed content.
- Local tampering or accidental edits to generated files.
- Updates introducing new tools, network dependencies, hooks, or privileged components.
- Overprivileged vendor integration credentials.
- Compromised client or service update delivery.
- Abuse of source-fetching and scanning infrastructure.

### Separate security evidence

| Evidence | Establishes | Does not establish |
| --- | --- | --- |
| Hash verification | Content matches an expected digest | Content is benign |
| Signature/attestation | A particular authority attested to a digest | Authority was uncompromised or content is safe |
| Human review | A reviewer approved a fixed candidate under a policy | Universal safety or unchanged behavior in every environment |
| Static scanning | Specific automated findings for a digest | Absence of all malicious instructions or runtime risk |
| Evaluation | Performance on specified scenarios | General safety or production causality |
| Runtime restrictions | An enforced execution boundary | That every vendor component is covered by that boundary |

### Release state machine

```text
draft -> quarantined -> validated -> in_review -> approved -> published
                        |              |            |
                        v              v            v
                     rejected    changes_requested revoked
```

Operational processing states such as scan pending, publish pending, retryable failure, and permanent failure are separate from approval. A release does not become approved because a worker finished. Content changes create a new candidate; a reviewer never approves a moving folder.

Publish and channel promotion must reference an approved effective digest and destination artifact lineage. If a destination transformer changes execution-relevant content or permissions, re-review that transformed result according to policy. Policy changes can invalidate eligibility without erasing historical approval records.

### Roles and separation of duties

Start with organization Owner/Admin, Maintainer/Author, Reviewer, Member, and Read-only Auditor permissions. Implement them as capabilities rather than scattering role-name checks.

- Authors import and submit candidates within their permissions.
- Reviewers approve eligible candidates for defined audiences.
- Members install permitted releases and report issues.
- Administrators configure sources, policies, retention, assignments, and integrations.
- Auditors inspect authorized metadata and records without mutating content.
- Managed policy should disallow self-approval by default for protected releases; exceptions require an explicit recorded rule. Personal/local use remains a different policy mode.
- Roles, groups, and approvals must be organization-scoped. A user belonging to two organizations cannot carry approval authority across them.

### Approval record

Bind approval to tenant, skill ID, effective digest, destination package or transformation policy, permitted audience, policy version, reviewer identity, decision time, and any expiry or conditions. Record scan/evaluation evidence IDs available at review time. New overlays, forks, or changed dependencies require the corresponding new decision.

### Enforcement and revocation

- V1 enforcement covers managed installs and publications. Unmanaged folders and arbitrary user edits are detectable only within scanned supported roots and access permissions.
- Clients refresh policy at defined intervals and before protected operations. Record policy age and last successful refresh.
- Define maximum offline policy age separately for new installs, updates, and existing use. A CLI installer alone cannot enforce a policy on every later runtime invocation.
- Distinguish service access revocation, future install blocking, local disablement, vendor disablement, running-session interruption, and artifact deletion. Report only actions actually supported and acknowledged.
- Keep revocation events and incident reason records. Do not delete audit evidence to make a compromised release disappear from history.
- Use platform permissions/sandbox controls for execution containment. Do not claim the management client is a complete runtime sandbox.

### Scanner/evaluation isolation

Static inspection and preview must treat content as data. Escape rendered Markdown/HTML and links, and avoid active previews. Optional model-assisted review must have no privileged tools, no source-controlled instructions promoted into system instructions, and explicit consent before private content is sent to an external model provider.

Any later execution-based evaluation runs in a disposable, resource-limited environment with explicit credentials and network policy, isolated from the control service and other tenants. Do not execute imported content on general service workers or developers' machines as part of an automatic scan.

## 11. Local mutation, recovery, and diagnostic design

### Shared operation engine

All mutating CLI, MCP, desktop, and background operations must share one engine:

1. Resolve authority and acquire coordination locks.
2. Read and validate current manifest, lockfile, local state, and policy.
3. Build an immutable plan with preconditions.
4. Validate approval, artifact identity, compatibility, and local modifications.
5. Stage new files without overwriting active content.
6. Persist a recoverable operation journal and required backups.
7. Apply filesystem/database transitions using defined ordering and atomic replacements where supported.
8. Verify the resulting deployment.
9. Record completion and release locks; retain recovery references under policy.

SQLite transactions cover database changes, not a multi-root filesystem transaction. Use a durable journal with explicit recovery actions. Do not label returned-error rollback “crash safe” until interruption tests pass.

Start with a simple process-wide coordination strategy for shared local database writes, including bootstrap, update checks, telemetry settings, and read operations that implicitly migrate/bootstrap. Analyze filesystem-only writers and vendor destination collisions too. Refine lock granularity only after correctness is established; define lock order before introducing multiple locks.

Avoid restoring a live shared SQLite database by uncoordinated file replacement. Use supported SQLite backup/recovery techniques and transactional migrations. Backups themselves need integrity verification and retention limits.

### Drift policy

Use the same dirty-content detection before sync, install-triggered sync, pin, rollback, remove, clean, and update apply. Generated status establishes ownership of a directory, not permission to discard unrecorded edits. Preserve edited content durably before any explicit discard. Preview all cleanup/removal actions and avoid deleting unmanaged or path-escaped files.

### Diagnostics

Expose separate per-destination fields:

- Desired release/effective digest.
- Planned root/path.
- Observed presence and readability.
- Observed digest and match state.
- Approval/policy eligibility.
- Vendor publication status, if applicable.
- Runtime discovery/activation evidence, if available.
- Last checked time and evidence source.
- Actionable repair or next step.

The human default should show the relevant reason and repair action without requiring JSON. Keep intended and observed state available in JSON. Unify CLI exit code, payload success/error state, and MCP error signaling; if diagnostics can succeed while reporting an unhealthy environment, name those separate concepts explicitly and test their mapping.

## 12. Hosted API and synchronization contracts

Exact endpoint names below are proposed. Finalize OpenAPI or an equivalent machine-readable contract before implementing clients. Version APIs from the first release.

| Capability | Proposed interface | Required behavior |
| --- | --- | --- |
| Membership | `/v1/organizations`, `/memberships`, `/groups` | Tenant-scoped authorization; invitations and revocation |
| Import | `/v1/imports`, `/imports/{id}` | Asynchronous status, safe retry, immutable result |
| Upload | `/v1/uploads`, `/uploads/{id}/finalize` | Bounded URLs; independent validation on finalize |
| Library | `/v1/skills`, `/skills/{id}/releases` | Stable identities and immutable release content |
| Review | `/v1/reviews`, `/reviews/{id}/decisions` | Fixed digest, policy check, concurrency control |
| Promotion | `/v1/channels/{id}` | Compare-and-swap pointer updates to approved releases |
| Policy/revocation | `/v1/policies`, `/v1/revocations` | Versioned policy; recorded effective time and scope |
| Artifact download | `/v1/artifacts/{id}/download` | Authorization and eligibility before signed URL issuance |
| Integrations | `/v1/integrations`, `/publications` | Server-owned credentials; explicit vendor operation state |
| Devices/inventory | `/v1/devices`, `/v1/installation-reports` | Enrollment, freshness, reconciliation, idempotency |
| Audit | `/v1/audit-events` | Authorized read/export; bounded pagination |
| Usage, later | `/v1/observations/batches` | Tenant validation, event IDs, schema version, late arrivals |

Requirements:

- Authenticate users independently of their selected organization; server verifies membership on every request.
- Use idempotency keys for imports, release creation, decisions, promotion, and publication. Reused keys with different content must fail explicitly.
- Use optimistic concurrency/version checks on mutable metadata and channel pointers.
- Return structured errors with stable codes, retryability, operation ID, and safe context. Never return credentials or raw secret-bearing upstream errors.
- Use cursor pagination and bounded batch sizes.
- Sign or otherwise authenticate release/policy manifests; plan key rotation and distinguish signing authority from approval authority.
- Treat device installation receipts as authenticated reports, not cryptographic proof that an uncompromised runtime obeyed policy.
- On logout, revoke/clear local service credentials and stop private sync. Do not silently delete user-authored work or claim downloaded artifacts have been erased everywhere.
- Do not allow account deletion or retention cleanup to orphan approvals/audit records without a documented pseudonymization and retention policy.

## 13. macOS application experience

### Main surfaces

- **Menu bar:** overall health, current organization, pending approved updates, failed actions, quick search, open library, pause background activity, quit.
- **Library window:** personal/team tabs, search, owner, release/channel, approval, compatibility, installed destinations, and last verification.
- **Skill detail:** purpose, origin, files, versions, dependencies, review evidence, changes, installations, and issues.
- **Review window:** complete bundle and diff, new capabilities, findings, approvals, explicit decisions.
- **Destinations:** detected tools, selected roots/scopes, vendor workspace connections, unsupported or manual actions.
- **Activity/recovery:** operation history, errors, retry, preserved local edits, rollback, and recovery status.
- **Settings:** account/org, permissions, proxy/network support, privacy, update policy, cache limits, logs, and integration health.

### Interaction principles

- The menu bar is an entry point; complex review happens in a normal window.
- Do not require users to learn “projection” or cache internals to install a skill.
- Display technical paths and digests when they support investigation or approval, with copy controls.
- Show actionable states: pending review, available, downloaded, installed locally, published, awaiting vendor/user action, verified, drifted, revoked, offline/unknown.
- Never display a green “installed everywhere” based only on package export.
- Use keyboard navigation, VoiceOver labels, scalable text, clear focus states, and system light/dark appearance.
- Background work is cancelable where safe; a cancellation during mutation transitions through recovery rather than leaving ambiguous state.
- Notify about actions users can take; aggregate repeated failures and avoid update notification spam.
- Provide a local-only onboarding path and a team path. Explain uploads before private local content leaves the Mac.

### Desktop implementation details

- Proposed layout: `apps/macos/Skillctl/` for the native app and its tests; `services/control-plane/` for the server; `packages/contracts/` for versioned schemas. Confirm repository organization before creation; do not move all existing Rust code merely for symmetry.
- Initial app-to-engine commands use a restricted versioned request model over pipes, with argument arrays rather than shell interpolation. Cap output sizes and cancel child processes cleanly.
- Keep logs in appropriate application support/log locations with redaction. Do not leak private skill contents into crash reports.
- Discover supported agent installations and let users override roots explicitly. Show uncertainty rather than silently guessing.
- Resolve macOS sandbox/bookmark and background-access requirements in the spike. Do not request broad disk or accessibility permissions without a concrete need.
- Sign/notarize the app and embedded executable, verify update signatures, and prevent executable downgrade to incompatible state schemas. Document rollback/update recovery.
- Select and document a minimum supported macOS version based on pilot hardware and required APIs; do not assume the latest OS.

## 14. Destination integration strategy

### Supported-state matrix

Maintain a machine-readable, versioned capability table alongside adapter logic:

| Destination | v1 path | What counts as success | Limit |
| --- | --- | --- | --- |
| Local Codex | Verified local roots and configuration | Expected artifact present and readable with matching digest | Not proof of invocation; other scopes/plugins may participate |
| Local Claude Code | Verified local roots or supported native distribution | Local verification and relevant runtime discovery evidence when available | Account sync and other skill sources can coexist |
| Local OpenCode | Verified roots/configuration | Local verification with documented precedence | Explicit config/catalogs can add or override skills |
| Other existing CLI adapters | Re-verify before retaining supported status | Same evidence contract | Do not claim support from stale static paths alone |
| ChatGPT local filesystem skills | Supported local capabilities only | Verified local artifact at supported roots | Separate from workspace skill/plugin installation |
| ChatGPT workspace | Supported GitHub marketplace sync or guided package handoff | Vendor publication/import acknowledgment and remote identity where obtainable | Admin permissions; availability varies by surface; sync is not instantaneous |
| Claude Desktop Chat/Cowork | Account-managed upload/distribution | Vendor acknowledgment or explicit pending manual handoff | Copying `~/.claude/skills` alone is not account publication |
| Claude Enterprise | Plugins API or supported marketplace integration | Remote immutable version ID plus served-version and audience state | Enterprise eligibility, scoped admin access, vendor scan and policy behavior |

### Vendor publishing rules

- Test with real eligible design-partner workspaces before advertising automatic deployment.
- Keep plan/permission prerequisites visible. A paid skillctl account does not create vendor Enterprise entitlements.
- Store vendor version IDs and package lineage, including any content transformation or rejected metadata.
- Prefer supported APIs and marketplace formats. Where only manual upload is supported, provide a generated package, clear instructions, and “awaiting confirmation” status.
- Do not treat the Claude model Skills API as equivalent to Claude consumer/Enterprise account plugin distribution.
- Do not assume an MCP connector installs native skills; loading workflow content through a tool is a separate integration with different discovery and policy semantics.
- Before promoting a Claude Enterprise API-uploaded version, account for served-version pinning and scan completion so an unready upload does not unintentionally displace the current release.
- ChatGPT's documented workspace GitHub sync can use fixed commits; verify whether publication/export packaging preserves all required metadata and how release promotion is represented.
- Resolve unexpected conflicting native installs explicitly. Do not let both skillctl and a vendor auto-updater silently own the same mutable local directory.

### Integration validation record

For each supported destination record tested date, documentation URLs, runtime/app version, plan, authentication mode, operating system, discovery roots, precedence behavior, packaging rules, install/update/rollback path, available runtime events, and known limits. Recheck at each release and at least quarterly. CI fixture tests protect logic, but do not substitute for vendor behavior checks.

## 15. Execution roadmap and dependencies

Effort estimates are rough focused engineering effort, including tests and documentation, not calendar commitments. Vendor approvals, customer recruitment, security review, and account eligibility can extend elapsed time. Estimate again after the first integration and desktop spikes.

| Package | Deliverable | Priority | Depends on | Rough effort | Initial status |
| --- | --- | --- | --- | --- | --- |
| W01 | Customer validation and desktop/vendor feasibility | P0 | None | 1–2 weeks | TODO |
| W02 | Clean verification baseline and accurate diagnostics | P0 | None | 3–5 days | TODO |
| W03 | Atomic migrations and coordinated recoverable mutations | P0 | W02 | 2–3 weeks | TODO |
| W04 | Canonical bundles and frozen empty-cache restoration | P0 | W03 | 1–2 weeks | TODO |
| W05 | Shared reviewed update/apply and consistent drift protection | P0 | W03, W04 | 1–2 weeks | TODO |
| W06 | Approval, policy, and portable contract models | P0 | W01, W04 | 1–2 weeks | TODO |
| W07 | Private team service, tenancy, artifacts, and imports | P1 | W01, W06 | 2–4 weeks | TODO |
| W08 | Review, release channels, audit, and incident controls | P1 | W05, W07 | 1–3 weeks | TODO |
| W09 | macOS companion and versioned local protocol | P1 | W01, W02, W05, W06 | 2–4 weeks | TODO |
| W10 | Vendor publishing integration and package handoff | P1 | W01, W07, W08 | 1–3 weeks per initial integration | TODO |
| W11 | Integrated macOS pilot, release, and support readiness | P1 | W08, W09, W10 | 2–3 weeks | TODO |
| W12 | Paid company inventory/compliance and administration | P2 | W11 and paid-pilot evidence | 3–6 weeks | TODO |
| W13 | Runtime observations and coverage-aware analytics | P2 | W12, validated runtime signals | 3–6 weeks | TODO |
| W14 | Evaluation and champion-to-team learning | P2 | W12, W13, customer evidence | 4–8 weeks | TODO |
| W15 | Windows and optional advanced deployment models | P3 | Stable v1 plus demonstrated demand | Separate estimate | TODO |

Suggested release stages:

- **Engine preview:** W02–W05; useful without accounts.
- **Private macOS team alpha:** W01, W06–W10 with foundation complete.
- **macOS v1:** W11 release gate passes; do not market future analytics as delivered.
- **Paid company beta:** W12 with explicit service terms and support expectations.
- **Learning/analytics expansion:** W13–W14, separately enabled and validated.

W01 and W02 can proceed independently. UI prototyping can overlap foundational work, but a polished UI is not a substitute for verified installation semantics. Cross-package work must have explicit file/module ownership and integration contracts. This roadmap does not itself require concurrent agent execution.

### W01 — Validate the customer and integration premise

**Owner/responsibility:** Product and integration lead. Own research records and disposable prototypes; do not change core lifecycle behavior.

**Steps:**

1. Run the customer validation in section 3 and record baseline workflows and measurable pain.
2. Prototype the smallest native menu bar entry, library window, browser sign-in callback, and Rust child-process interaction.
3. Test required filesystem access and signing/distribution assumptions on pilot macOS versions.
4. Publish one harmless test skill through a supported ChatGPT workspace route and one Claude account/Enterprise route using authorized test workspaces.
5. Record supported automation, manual handoffs, remote identities, update/rollback semantics, and evidence gaps.
6. Select the first automatic vendor integration based on pilot usage, not implementation novelty.
7. Write ADRs for desktop stack, identity, backend host, object store, initial source host, and initial vendor path.

**Acceptance:** A new team member can follow the recorded prototype steps without undocumented manual state edits. A capability matrix identifies verified automatic paths and explicitly manual ones. At least three qualified design partners are recruited before a broad hosted build.

**Stop/re-scope:** If core targets require undocumented app modification, lack eligible customer accounts, or require rights customers cannot grant, narrow the advertised support. If users predominantly need cloud distribution, reassess whether a mandatory desktop install is justified.

### W02 — Establish a clean baseline and truthful diagnostics

**Scope:** `src/error.rs`, `src/lifecycle.rs`, `src/source.rs`, `src/response.rs`, `src/runtime.rs`, `src/doctor.rs`, `src/mcp.rs`, `src/adapter.rs`, related tests, CI/configuration, and relevant docs. Do not redesign the backend or UI here.

**Steps:**

1. Write regression tests for conflict results across CLI JSON, process exit, and MCP.
2. Add per-destination observed presence/digest state and replace ambiguous visibility claims. Test missing files, unreadable roots, conflicts, unsynced canonical content, and multiple scopes.
3. Surface actionable diagnostic fixes in ordinary human output.
4. Fix the existing Clippy baseline using the smallest supported changes; document the supported Rust toolchain and test upgrade policy.
5. Re-verify adapter facts and introduce verification metadata/fixtures without forcing a wholesale adapter rewrite.
6. Keep old JSON consumers compatible through versioning or explicit migration notes.

**Verification:** All existing Rust gates in section 21 pass; relevant new CLI and MCP tests pass; a missing target projection never reports observed installation as healthy.

**Risk:** Medium for response compatibility. Prefer additive explicit fields over silently changing meanings.

### W03 — Make state migration and mutation recoverable

**Scope:** `src/state.rs`, `src/state/schema.rs`, `src/state/tests.rs`, `src/lifecycle.rs`, mutating callers, and integration tests. This package owns locking/journaling contracts used by all later writers.

**Steps:**

1. Write failing tests for interruption/error before and after table renames, schema creation, data copies, cleanup, and version advancement.
2. Execute migration steps and version advancement in a single supported SQLite transaction.
3. Detect already-stranded databases with legacy and current tables. Back up safely and recover only when row ownership and reconciliation are unambiguous; otherwise return an actionable non-destructive recovery state.
4. Introduce process coordination around all shared database/filesystem mutation, including implicit bootstrap and settings writes.
5. Replace uncoordinated whole-database restoration with a supported coordinated strategy.
6. Add a durable operation journal and staged replacement, defining recovery after termination at each transition.
7. Implement a bounded recovery/diagnostic entry point with preview before destructive repair.
8. Define backup retention, lock timeout, stale process handling, and no-partial-success reporting.

**Verification:** Two processes in different test workspaces cannot erase each other's committed history when one fails. Terminating a test child at each journal boundary leaves either the old state, the new state, or an explicit recoverable operation. Migration tests compare row counts and identities, not only schema version/table presence.

**Stop condition:** Never guess which workspace owns ambiguous legacy records or discard a user's existing database to make a test pass.

### W04 — Define bundles and restore from an empty cache

**Scope:** `src/source.rs`, `src/resolver.rs`, `src/lockfile.rs`, `src/manifest.rs`, hashing/bundle module to be introduced, `src/cli.rs`, and integration fixtures/tests.

**Steps:**

1. Specify the canonical bundle format and hashing rules from section 9; add deterministic fixtures and cross-platform collision cases.
2. Add immutable local artifact storage while retaining compatibility with supported prior installs.
3. Design and migrate the portable lockfile, separating local installation bookkeeping from dependency identity.
4. Add exact-revision acquisition and cached-artifact verification. Never substitute the current branch HEAD for a missing locked revision.
5. Implement frozen restore and read-only verification with explicit offline behavior.
6. Support retained folder/ZIP snapshots; make nonportable legacy sources actionable rather than silently reproducible.
7. Document clone-and-restore, moving a checkout, restoring after cache eviction, and upstream unavailability.

**Verification:** Install a source, commit only the declared portable state, change upstream, and restore in a different path with a new isolated test home. Content matches the original approved digest and the lockfile is unchanged. A modified artifact or unavailable revision fails without advancing dependency state.

**Maintenance:** Hash format version is a compatibility boundary. Future normalization changes require migration, not silent digest reinterpretation.

### W05 — Complete updates through a shared guarded apply path

**Scope:** `src/planner.rs`, `src/history.rs`, `src/materialize.rs`, `src/lifecycle.rs`, `src/overlay.rs`, `src/skill.rs`, `src/cli.rs`, shared response/MCP paths, and lifecycle tests.

**Steps:**

1. Define a typed immutable plan with expected current digests, lockfile state, targets, proposed changes, approval requirements, and recovery references.
2. Reuse common execution code for update, pin, rollback, sync-triggered changes, enable/disable, removal, and cleanup where appropriate.
3. Centralize dirty-content protection before all destructive writes.
4. Implement clean update apply, preserve-as-overlay, explicit detach/fork submission, skip, and explicit discard with durable backup.
5. Recompute effective content and require policy eligibility for overlays/forks; preserve local ownership separately from organizational approval.
6. Reject stale/tampered plans and recover failed or interrupted multi-target operations.
7. Add concise diffs and machine-readable result semantics across CLI/MCP.

**Verification:** A direct edit survives an unrelated install and sync unless explicitly discarded. Update applies an approved new revision, records history, and rolls back to a retained eligible release. Changing local state after planning causes a stale-plan error before mutation. All applicable Rust gates pass.

### W06 — Specify approvals, policy, and public contracts

**Scope:** Rust trust/domain types, portable manifest/lock contracts, `packages/contracts/` schemas and examples, and architecture decision records. Do not add billing or runtime analytics here.

**Steps:**

1. Define identities and source-of-truth rules from sections 8–10.
2. Separate source ownership, content integrity, human approval, scan result, and runtime containment in types and responses.
3. Version the local app protocol, hosted API, event schema, policy manifest, and canonical bundle manifest.
4. Define role capabilities, protected release policies, offline behavior, and revocation semantics.
5. Specify interoperability/import behavior for existing skills tooling and native plugin formats. Prefer preserving provenance and exporting useful content over a proprietary lock-in requirement.
6. Add schema examples for personal imports, shared releases, overlays, denied installs, and vendor publications.

**Verification:** Machine validation accepts valid fixtures and rejects wrong-tenant approvals, changed content with an old approval, missing lineage, unsupported protocol versions, and an approval copied across organizations. Publish exact validation commands in the package's README before dependent implementation starts.

### W07 — Build private team storage, membership, and ingestion

**Scope:** New control service, database migrations, identity integration, artifact storage, source acquisition/validation workers, contract tests, and local client authentication. Confirm concrete directories in W01/W06.

**Steps:**

1. Bootstrap the selected backend with one documented local development/test command and a deployment configuration; defer actual deployment until authorized.
2. Implement organizations, invitations, membership lifecycle, roles, and server-side tenant checks.
3. Implement browser-based native sign-in and secure token storage; add CLI authentication only where needed.
4. Add upload sessions, quarantined ingestion, bounded ZIP handling, GitHub integration, and candidate discovery.
5. Build canonical bundles and retain immutable artifacts without executing imported content.
6. Implement authorized artifact download and client verification.
7. Add idempotency, request correlation, safe errors, quotas, and import cancellation/cleanup.
8. Add artifact/database backup and a restore exercise before accepting important customer data.

**Verification:** Automated two-tenant tests prove inaccessible IDs, altered tenant selectors, replayed signed URLs outside their authorization bounds, invitation revocation, expired sessions, and source-fetch restrictions fail safely. Import retry does not create duplicate releases or overwrite approved content. Unsafe archives never escape quarantine.

**Risk:** High because this introduces an internet-facing multi-tenant service. Require an independent scoped security review before customer production use.

### W08 — Add release review, channels, audit, and revocation

**Scope:** Control-service release/review/policy modules, worker publication queue, app/CLI contracts, audit store, and tests. Keep analytics limited to operational deployment facts.

**Steps:**

1. Implement immutable candidate submission and complete file/diff review.
2. Store scanner results as versioned evidence; choose external scanners only after a private-data handling decision.
3. Implement reviewer decisions, role checks, separation of duties, and policy-bound approvals.
4. Implement release channels and audience assignment with concurrency-safe promotion.
5. Add append-only application audit records and privileged audit exports.
6. Add revocation, replacement selection, client policy refresh, and outstanding-remediation inventory.
7. Handle approval expiry, reviewer departure, policy changes, and failed publishing without rewriting history.

**Verification:** Editing a submitted candidate cannot alter what the reviewer approves. Changing effective content invalidates old eligibility. A revoked digest cannot be newly installed or selected as an unauthorized rollback. An offline device remains outstanding until it reports a successful remediation. Tests cover simultaneous reviewers and duplicate promotion retries.

### W09 — Ship the macOS companion

**Scope:** Native app and tests under the W01-approved directory, bundled CLI packaging, versioned local protocol, authentication UI, and app-specific CI.

**Steps:**

1. Build menu bar status, library window, local-only onboarding, and team sign-in.
2. Add GitHub input, ZIP drag/drop, folder selection, multi-skill selection, and explicit publish/upload controls.
3. Add skill details, changes, compatibility, approvals, destination selection, and operation progress.
4. Add review UI or an authenticated web review handoff if that is the smaller complete v1; record the choice.
5. Implement install/update/rollback through the shared engine; never add a second filesystem writer.
6. Add background inventory, policy freshness, controlled notifications, cache settings, and recovery UI.
7. Add accessibility, redacted diagnostics export, app signing/notarization, and signed updates.

**Verification:** UI tests cover sign-in cancellation, expired login, organization switching, no network, access-denied folders, install cancellation, incompatible engine protocol, preserved edits, and revoked releases. Manual QA verifies VoiceOver, keyboard-only operation, launch/quit behavior, and clean-machine installation. Record `xcodebuild` scheme/destination commands after project creation; do not invent them in advance.

### W10 — Prove vendor publishing and handoff

**Scope:** Capability matrix, destination package builders, backend integration credentials, publication workers, native/guided UI, and vendor contract fixtures.

**Steps:**

1. Revalidate documentation/account prerequisites and the W01 feasibility findings.
2. Implement the selected automatic integration: Claude Enterprise Plugins API or supported ChatGPT workspace GitHub publishing bridge.
3. Generate destination packages from approved content and retain mapping from source/effective digest to package digest and remote version ID.
4. Implement status reconciliation, retries, bounded polling, permission failure handling, and rollback/promotion behavior.
5. Provide supported package export and accurate pending handoff status for other desktop targets.
6. Test conflicts with existing native libraries and clearly assign ownership for updates.

**Verification:** Publish, update, and roll back a harmless skill in an authorized eligible workspace. Record remote IDs, selected served version, audience, observed availability, and limitations. Simulate lost API responses, expired credentials, rate limits, scan pending/failure, and vendor downtime. Never claim member installation or invocation from a publication response alone.

**Stop condition:** If no supported automated path is verified for a surface, ship a clearly manual integration or remove the claim; do not automate undocumented application state.

### W11 — Run a paid-readiness macOS pilot and release v1

**Scope:** End-to-end integration, onboarding/docs, release packaging, support/recovery procedures, performance budgets, and pilot instrumentation.

**Steps:**

1. Run the section 5 end-to-end demonstration with at least three teams.
2. Test upgrades from the current CLI and from earlier app alpha versions without losing user-authored files or history.
3. Rehearse revoked-release remediation, lost network, service outage, backup restoration, and client update rollback.
4. Complete an independent security review of ingestion, tenancy, auth, approval integrity, client updates, and publishing credentials.
5. Publish accurate support boundaries, data handling, incident contact, known limitations, and pricing/pilot terms if charging.
6. Confirm usefulness compared with existing Git/native/vendor tooling using observed onboarding and update workflows.

**Acceptance:** Section 22 launch gates pass. Customers can export their own skills and provenance, leave the service, and retain local authoring. The product does not require analytics collection to use local lifecycle features.

### W12 — Launch paid company administration and inventory

**Scope:** Company administration, billing/entitlements, inventory/reporting, policy/audit exports, and required identity/lifecycle integrations. Requires actual paid-pilot evidence.

**Steps:**

1. Establish which administrative capabilities customers will pay for before expanding pricing tiers.
2. Add organization/group/project inventory, desired-vs-observed comparisons, approved-version compliance, freshness, and integration health.
3. Add retention controls, export/deletion workflows, role-based reporting, and documented tenant data boundaries.
4. Add SSO or provisioning integrations when a paying customer needs them; do not build every enterprise feature speculatively.
5. Implement billing and entitlement checks without trapping local authored work when a subscription ends.
6. Provide staged rollout, channel ownership, exception tracking, and incident reports.

**Verification:** Seat/membership changes reconcile predictably. Billing retries do not mutate artifacts or grant cross-tenant access. Dashboards distinguish stale/offline inventory from noncompliance. Cancellation retains documented export access and applies the chosen retention policy.

### W13 — Add runtime observations and honest usage analytics

**Scope:** Runtime adapters, event ingestion, tenant event store/aggregations, privacy controls, and analytics UI. Do not infer success from activation.

**Steps:**

1. Implement one verified runtime signal first, likely Claude Code's documented activation telemetry for eligible deployments.
2. Map observations to stable skill identity, effective/destination digest where available, session, runtime version, and event provenance.
3. Record unsupported/disabled collection and attribution ambiguity explicitly.
4. Add deduplication, offline buffering, ordering/late-arrival handling, redaction, and retention enforcement.
5. Add team-level reports for activation, adoption among observable sessions, update uptake, version distribution, and coverage.
6. Integrate vendor analytics APIs where available rather than maintaining fragile transcript scraping as the default.

**Verification:** Replaying an event batch does not double counts. A renamed or same-name skill is not silently attributed to the wrong release. Missing telemetry does not become zero usage. Privacy fixtures prove content and disallowed identity fields are omitted. Report formulas and coverage are reproducible from test datasets.

### W14 — Learn from champion users and evaluate outcomes

**Scope:** Opt-in contribution, evaluation infrastructure, evidence review, pilot cohorts, approved promotion, and outcome dashboards.

**Steps:**

1. Let champions explicitly nominate a workflow or skill variant, with ownership and sharing rights confirmed.
2. Capture a sanitized description, intended task, examples, dependencies, and known limitations. Do not silently mine private conversations.
3. Create versioned evaluation scenarios and task-specific outcome criteria.
4. Compare candidate and baseline under recorded agent/model/tool configurations, preferably with randomized or matched comparisons when feasible.
5. Review security, compatibility, and outcome evidence before sharing beyond the pilot cohort.
6. Publish an approved release, deploy to a pilot group, and collect supported observations and explicit feedback.
7. Promote, revise, or withdraw based on evidence; retain rollback and attribution.

**Verification:** Permission-denied content never enters candidate extraction or evaluations. Evaluation reruns retain dataset/config identity. Reports distinguish correlation, self-report, and controlled comparison. A champion's higher usage alone cannot trigger automatic organization-wide promotion.

### W15 — Expand only against demonstrated demand

Possible work: Windows tray UI and credential storage, managed-device deployment, more vendors, customer-controlled storage, private networking, advanced attestations, and self-hosted control service.

First evaluate cross-platform filesystem semantics, case collisions, path limits, execution attributes, signatures, installer/update behavior, and shared protocol compatibility. Keep the portable artifact and policy contracts stable. Do not assume that a native macOS UI can be reused directly on Windows; the reusable investment is the Rust engine and service contracts.

## 16. Future paid company service

### Commercial value

The paid service should sell reduced operational risk and better reuse of company knowledge:

- Onboard teams onto a consistent set of approved workflows.
- Keep projects on known eligible releases with visible exceptions.
- Review updates once and distribute them through several agent ecosystems.
- Find compromised, unsupported, obsolete, or abandoned skills and remediate them.
- Understand which workflows are used, under which observable conditions.
- Identify promising practices from champions and evaluate whether they benefit other teams.
- Preserve ownership, attribution, approvals, and outcome evidence as organizational knowledge.

Do not sell “number of installs” as proof of return on investment. The company service must connect operational facts to task-specific evidence, with uncertainty and collection coverage visible.

### Packaging hypotheses

| Offering | Intended customer | Candidate capabilities |
| --- | --- | --- |
| Local/free | Individuals and evaluators | CLI/local lifecycle, local app library, verification, export, diagnostics |
| Team | Mixed-agent teams | Private library, roles, approvals, channels, shared artifacts, vendor publishing, basic inventory |
| Company | Platform/security and multiple teams | Policy, company inventory, audit, group rollout, revocation, analytics, retention controls, evaluation/contribution workflows |
| Enterprise, only when needed | Organizations with specific constraints | Advanced identity, provisioning, contractual support, customer-controlled storage/deployment options, additional compliance requirements |

These are hypotheses, not committed editions. Avoid gating basic integrity checks or making local recovery depend on a paid subscription. Commercial entitlements should control collaboration/service capabilities, not access to a user's own authored skills.

### Pricing discovery

- Start with paid design-partner pilots and a clear bounded scope.
- Test a company base subscription plus managed active users against a simple per-active-user alternative.
- Do not charge per activation initially; it discourages use and makes bills harder to predict.
- Evaluation/model execution may need separately disclosed usage costs or customer-provided credentials. Estimate and cap spending before runs.
- Track service costs: artifact storage/egress, scan work, vendor API usage, event ingestion, evaluation runs, and support time.
- Select prices after willingness-to-pay and unit-cost evidence. This plan deliberately does not invent price points or revenue forecasts.
- Separate skillctl billing from vendor subscription requirements in the purchase flow.

### Administration and operations prerequisites

Before general paid availability, implement tenant-safe support tooling, audit access, backup restoration, incident handling, customer export/deletion, abuse controls, billing reconciliation, and a clear service status/support channel. Commit only to availability and support levels the operating team can sustain. Define data residency and external-processing behavior accurately; do not claim certifications or residency coverage merely because the object store supports a region.

## 17. Observability and analytics specification

### Evidence layers

| Layer | Example evidence | Valid question | Invalid inference |
| --- | --- | --- | --- |
| Catalog | Approved release and audience | What may this group install? | Everyone has installed it |
| Deployment | Verified local files or vendor publication state | What content is deployed or reported? | The agent loaded it |
| Discovery | Supported runtime catalog/loading evidence | Was it available to this runtime/session? | It was selected |
| Invocation | Explicit runtime activation event | Was it invoked in observed sessions? | It completed successfully |
| Outcome | Tests, task result, user feedback, evaluation | Did the observed task meet defined criteria? | The skill caused all improvement |

Every dashboard should expose its evidence layer, time window, source, and coverage limitations.

### Event envelope

Define a versioned minimal event schema with:

- Event ID, schema version, event type, occurred time, received time.
- Tenant, authorized integration/device identity, and appropriately scoped user/team identity where enabled.
- Project/repository pseudonymous identifier, subject to customer policy.
- Session/task correlation ID with documented stability and retention.
- Skill ID, release/effective/destination digest when known, and attribution confidence.
- Runtime family/version, model identifier when available, source scope, and invocation trigger.
- Outcome reference/status only when observed; never fabricate a skill completion from session completion.
- Collector/version, direct-vendor vs plugin-derived vs inferred provenance, and relevant configuration/coverage.

Keep raw prompts, source files, tool arguments, secrets, private repository URLs, and arbitrary transcripts out of default collection. Model/provider identifiers can themselves reveal business-sensitive use; apply the customer's policy.

### Initial events

Start with operational events: import requested/completed/failed, candidate submitted, approval/rejection, release published, channel promoted, installation verified/drifted, update applied/failed, release revoked, remediation acknowledged, and policy refresh.

Later add observed skill activation, runtime discovery where available, explicit feedback, evaluation result, and task outcome linkage. Avoid a generic `skill_succeeded` event without a defined producer and criterion.

### Initial company dashboards

1. **Inventory:** approved, desired, published, and observed versions by team/project/destination, with last-seen age.
2. **Compliance:** eligible release adoption, drift, revoked releases, unsupported dependencies, and approved exceptions.
3. **Lifecycle:** update lag relative to approved channel, failed updates, rollback, abandoned ownership, and integration failures.
4. **Usage:** activation by skill/release/runtime among observable sessions; explicit telemetry coverage.
5. **Quality:** scenario results, user feedback, incidents, and documented outcome signals.
6. **Learning:** nominated practices, review/evaluation progress, pilot adoption, and evidence for promotion.

### Metric definitions

- **Approved-version compliance:** fresh observed eligible installations divided by fresh observed in-scope installations; report stale/unknown installations separately rather than dropping them invisibly.
- **Update lag:** time since the approved channel changed, not time since upstream HEAD changed. A deliberately pinned project may be compliant.
- **Activation rate:** sessions with an observed activation divided by observable eligible sessions, only if that denominator can be established. Otherwise show raw observed activations and coverage.
- **Remediation progress:** acknowledged eligible replacement/disablement among known affected installations, plus outstanding/offline counts.
- **Success rate:** tasks meeting a named outcome criterion divided by tasks evaluated under that criterion; show sample size, selection method, missing outcomes, and relevant configuration.
- **Outcome change:** difference against a recorded baseline, with confounders and uncertainty. Do not automatically label an observational difference causal.

### Privacy and trust

- Separate three concerns: operational security/audit records, customer-controlled usage analytics, and optional vendor/product-improvement telemetry.
- Product-improvement telemetry should be off in the new team product until explicitly opted into; this is a proposed change from the current spec's opt-out public-source telemetry. Record and migrate that decision deliberately.
- Customers control analytics enablement, retention, identity visibility, external model processing, and authorized audiences, within documented mandatory audit requirements.
- Members must receive a clear description of what the company collects. Do not repurpose security logs into hidden performance rankings.
- Prefer team aggregates and minimum cohort sizes for broad reports. Restrict person-level incident investigation to explicit authorized roles and recorded access.
- Determine legal/contractual requirements with qualified review before production; this document is not a compliance certification.

## 18. Champion-to-team learning system

### Intended loop

```text
Useful individual practice
  -> explicit nomination and sharing consent
  -> sanitized reusable candidate
  -> ownership, security, and compatibility review
  -> baseline and evaluation scenarios
  -> limited pilot cohort
  -> measured feedback and outcomes
  -> approved team release
  -> continued monitoring, revision, or withdrawal
```

This loop should help companies capture practical knowledge without forcing everyone to adopt a champion's personal setup. The product's value is making the practice understandable, testable, and maintainable.

### Contribution record

Capture the problem solved, intended user/task, prerequisites, author/maintainer, explicit sharing rights, source provenance, selected supporting examples, known failure cases, security-sensitive capabilities, and proposed evaluation criteria. Preserve author attribution while ensuring the team has a durable owner if the champion changes roles or leaves.

Do not automatically publish user-global skills, private project content, customer material, or conversation excerpts. Suggest a contribution only from sources the customer and contributor have authorized for that purpose. Public/community publication is a separate explicit permission, not a side effect of company sharing.

### Finding promising practices

Possible signals include voluntary nominations, repeated favorable feedback, reusable workflows across projects, lower update/support burden, and evaluation improvement. Usage alone can reflect task mix, skill visibility, mandatory installation, or the champion's experience.

Show why a candidate was suggested, which evidence is direct vs inferred, and what remains untested. A recommendation should invite investigation, not silently become policy.

### Success analysis

- Define success per workflow: tests pass, release checklist complete, reviewer accepts output, incident task resolves correctly, or another explicit criterion.
- Compare like tasks and comparable configurations. Record model, runtime, tools, permissions, and repository context as appropriate.
- Use baseline/no-skill and prior-version comparisons where feasible.
- Report sample sizes and uncertainty; small pilot results are exploratory.
- Treat user feedback as self-report, not objective ground truth.
- Measure failure modes as well as average scores: unsafe action requests, missing prerequisites, wrong applicability, and excessive cost/latency.
- Retain regression scenarios with releases and rerun on meaningful skill or runtime/model changes.
- Include rollback and withdrawal when a practice proves harmful outside its original context.

### Product boundaries

The learning service may suggest description changes, candidate improvements, or related skills. Suggestions remain candidates requiring the normal review and evaluation flow. It must not self-modify approved production releases or use one company's private workflow to train or recommend to another company without separate authorization and a documented processing policy.

## 19. Success metrics and decision gates

### Pilot measurements

Before starting each pilot, record a baseline for onboarding a new teammate, installing a shared skill into two targets, reviewing an update, preserving customization, and locating the active version. Compare observed time, errors/retries, and confidence after the workflow, not just satisfaction ratings.

Working targets to validate, not service guarantees:

- An ordinary user can install an approved local skill without terminal use.
- A new teammate reaches a verified local installation within ten minutes after required vendor access is available.
- An existing local artifact installation completes within ten seconds on a representative pilot Mac, excluding vendor publication/network scanning delays; measure p50/p95 and revise budgets based on evidence.
- Every managed install has an expected digest, observed result, and recoverable operation record.
- No destructive edit loss or cross-tenant access in the launch acceptance suite.
- At least two of three pilot teams ask to continue, with a specific recurring use case.

The long-term company outcome is repeatable adoption of effective approved practices, not maximal skill count. Track inactive or redundant skills as a maintenance opportunity; do not encourage indiscriminate installation that overwhelms agent discovery budgets.

### Commercial continuation gates

- **After W01:** Is there a repeated mixed-agent problem beyond native vendor libraries?
- **After W05:** Can the engine uphold reproducibility and recovery on its own?
- **After W11:** Do teams continue using the complete workflow and identify a buyer?
- **Before W13:** Is there reliable runtime evidence and a customer-approved collection policy?
- **Before W14:** Can a task-specific outcome be measured, and do champions want to contribute?
- **Before W15:** Does expansion unblock paying use rather than merely increase platform count?

If a gate fails, document the evidence, narrow the product, or pause that expansion. Do not build every future tier to validate an unproven initial job.

## 20. Risks, alternatives, and non-goals

| Risk | Mitigation or decision |
| --- | --- |
| Native vendors close the gap | Integrate with their distribution; prove mixed-vendor value and avoid duplicating single-vendor administration |
| Competing managers already suffice | Benchmark actual pilot workflows; support import/export and narrow to a verified advantage |
| Integration behavior changes | Versioned capability records, conformance fixtures, regular live checks, truthful degradation |
| Desktop app adds little value | Make server publishing and CLI usable independently; validate the app's recurring local job |
| Overclaiming security | Digest approval plus evidence and enforcement boundaries; no “safe because scanned” label |
| Compromised control plane/signing authority | Scoped access, key management/rotation, audit, independent review, artifact lineage, incident rehearsal |
| Offline clients miss revocation | Explicit freshness/lease policy and outstanding state; no guaranteed universal instant revocation |
| Unmanaged runtime copies | Detect supported roots, report coverage, integrate managed runtime policy where available |
| Update destroys edits | Shared drift guard, durable backup, tested journal recovery |
| Analytics becomes surveillance | Tenant policy, transparency, limited access, team aggregates, no default individual rankings |
| Usage is confused with value | Separate evidence layers; versioned outcome criteria and evaluations |
| Scanning/evaluation leaks private content | Explicit processing consent, isolation, retention controls, provider choices |
| Large or malicious imports exhaust resources | Bounded ingestion and execution isolation; quarantine separate from approved storage |
| Service dependency undermines local use | Local-only mode, cached verified artifacts, export, documented offline policy |
| Product expands into a harness manager | Keep skills central; package related dependencies only as required for supported distribution |

Considered and intentionally rejected:

- Rewriting the Rust core solely to share a UI language: reuse the engine and contracts instead.
- Treating the read-only TUI as a defect: it remains useful and does not need feature parity with a desktop authoring/review experience.
- Building Windows simultaneously: defer until macOS workflows and contracts are stable.
- Launching a public registry first: private team value and safe lifecycle come first.
- Claiming all successful tests prove the product contract: add fresh-cache, concurrent-process, interruption, and tenant-boundary tests.
- Treating a fork as automatically approved: content/ownership changes require applicable review.
- Treating directory projection as proof of runtime loading: preserve separate evidence states.
- Building a proprietary scanner or generic agent-observability platform first: integrate existing evidence sources and focus on skill identity/version lineage.

## 21. Engineering workflow and verification

### Repository conventions

- Keep Rust modules domain-oriented, typed, and documented; `src/lib.rs` denies missing docs and broken rustdoc links.
- Follow `Result<T, AppError>` and existing structured `AppResponse` patterns, correcting their semantic inconsistencies deliberately.
- Match test patterns in `tests/cli/update.rs` and helpers in `tests/support/mod.rs`. Use isolated test workspaces, homes, and local fixture repositories; never depend on a developer's real skills or credentials.
- Follow the existing TDD requirement: write failing tests first for behavior changes.
- Use conventional commit messages consistent with history (`fix(state): ...`, `feat(cli): ...`).
- Keep compatibility and migration fixtures for persisted state and public machine contracts.
- Do not add a broad refactor to a bounded work package without explaining why its acceptance criteria require it.

### Verified existing commands

| Purpose | Command | Required result |
| --- | --- | --- |
| Format | `cargo fmt --check` | Exit 0 |
| Lint | `cargo clippy --all-targets --all-features --locked -- -D warnings` | Exit 0 after W02 |
| Test | `cargo test --locked` | All tests pass |
| Focused lifecycle CLI tests | `cargo test --locked --test cli` | All selected integration tests pass |
| MCP contracts | `cargo test --locked --test mcp` | CLI/MCP semantics agree |
| Compatibility fixtures | `cargo test --locked --test integration_matrix` | Fixture matrix passes |
| Release fixtures | `cargo test --locked --test release` | Packaging/release fixtures pass |
| Docs | `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps` | Exit 0 |
| Build release | `cargo build --release --locked` | Exit 0; only when producing/testing a release |

New backend, schema, and macOS projects must document exact setup, lint, typecheck, unit/integration test, build, and local-run commands when scaffolded. Add them to CI in that work package. A TODO command that nobody has executed is not a verification gate.

### Essential additional test suites

- Restore into an empty cache, moved workspace, and unavailable upstream.
- Bundle reproducibility and tamper detection, including executable attributes and path collisions.
- Two-process shared-state mutations and failures across workspaces.
- Process termination at migration and filesystem journal boundaries.
- Dirty generated files under every mutating command, including unrelated installation and cleanup.
- CLI/JSON/MCP/native protocol consistency and version mismatch rejection.
- ZIP traversal, links, collisions, malformed metadata, expansion limits, and source-fetch restrictions.
- Two-tenant resource and artifact authorization, membership revocation, and idempotent API retries.
- Changed-content approval invalidation and self-approval policy.
- Vendor publish/update/rollback contract failures and explicit manual states.
- Desktop sign-in, file access, offline behavior, accessibility, and upgrade recovery.
- Analytics duplicate/late/missing events, attribution collisions, coverage, retention, and redaction.
- Evaluation contamination, unauthorized input, and champion-sharing permission boundaries.

For security testing, use dummy principals and authorized isolated environments. Never validate tenant isolation or harmful content against unrelated production users. Imported executable content requires an explicit sandboxed evaluation environment, not ordinary test execution on the host.

### Git and worktree discipline

Before implementation, run:

```sh
git status --short
git diff --stat f9fd697..HEAD -- src tests Cargo.toml .github
```

Review drift in each work package's scope before using this baseline. Preserve unrelated uncommitted and untracked work, especially `docs/plan.md`.

Use worktrees under `~/dev/.worktrees/skillctl/<task>`. Inspect uncommitted, untracked, and ignored files before removing a completed worktree; preserve needed artifacts and never force removal to discard work. Do not push, publish, or deploy merely because a work package is ready; follow the operator's authorized scope.

## 22. macOS v1 launch checklist

All required items must have evidence, not just checked boxes:

- [ ] W01 validation and supported destination matrix are recorded.
- [ ] Rust baseline gates pass, including strict Clippy.
- [ ] Fresh-cache frozen restoration reproduces exact approved content.
- [ ] Migrations are atomic and known partial states recover non-destructively.
- [ ] Concurrent operations and forced termination tests pass.
- [ ] All destructive writers share dirty-content protection and recovery.
- [ ] Canonical digests include all distributed content and relevant file attributes.
- [ ] Approvals bind to fixed effective content and applicable destination lineage.
- [ ] Tenant, membership, artifact, source-ingestion, and publication authorization tests pass.
- [ ] The app signs in securely and does not embed vendor administrator credentials.
- [ ] The app and bundled engine are signed/notarized with a verified update path.
- [ ] At least one organization publishing integration is verified, or the product explicitly launches with manual handoff only and adjusts its claims.
- [ ] Desktop targets show accurate publication/installation/verification states.
- [ ] Backup restoration and revoked-release response are rehearsed.
- [ ] Privacy, retention, private-content processing, ownership, and export are documented.
- [ ] At least three pilot teams completed the primary workflow and continuation evidence is reviewed.
- [ ] Support, incident contact, known limits, and operational ownership are assigned.
- [ ] Future usage/success analytics are clearly labeled as future work unless implemented and validated.

## 23. Open decisions and next actions

### Decisions to resolve in W01/W06

1. Which initial teams and roles experience the strongest repeated pain?
2. Is the first automatic integration Claude Enterprise, ChatGPT GitHub marketplace publishing, or another supported route customers can actually authorize?
3. Does v1 need a web review console, or is native review plus a minimal web admin surface sufficient?
4. Which minimum macOS version, signing/update mechanism, and filesystem entitlement model fit pilots?
5. Which backend/identity/object-store providers meet customer constraints at acceptable complexity?
6. Which approval policies are mandatory in managed mode, including self-approval and emergency exceptions?
7. What offline policy ages and revocation limits will the first customers accept?
8. What artifact, audit, identity, and later usage-event retention periods are appropriate?
9. Which private-source access method and vendor credential scopes are acceptable?
10. Which existing lockfile/plugin formats should be imported/exported first?
11. Which runtime evidence can be collected reliably without intrusive instrumentation?
12. Which company workflow has an objective outcome suitable for the first evaluation pilot?

### Immediate implementation order

1. Accept or amend this roadmap's scope and proposed architectural choices.
2. Open bounded W01 and W02 implementation/research tasks, with explicit owners.
3. Fix verification and response/diagnostic semantics.
4. Complete migration/concurrency/recovery work before adding new mutation entry points.
5. Implement canonical artifacts, frozen restoration, and a shared guarded apply path.
6. Validate the desktop/vendor prototype before committing to the full hosted product.
7. Implement team approval/distribution and macOS onboarding; run the end-to-end pilot.
8. Use paid-pilot evidence to choose the first company analytics and champion-learning feature.

### Maintenance of this roadmap

Update work-package status and evidence after each completed milestone. Re-estimate dependencies when vendor capabilities or customer priorities change. Preserve rejected approaches and reasons so they are not repeatedly rediscovered. A package is complete only when its acceptance tests and relevant launch gates pass; a merged implementation alone is insufficient.

## 24. References and evidence

Vendor documentation checked on 2026-11-05; these are capability references, not permanent contracts. Fetch current pages again before implementing or making launch claims.

### Repository references

- `README.md`: current installation, commands, telemetry statements, and development checks.
- `spec.md`: original product definition, domain vocabulary, engineering/TDD expectations, and earlier non-goals.
- `docs/broader-product-completion-note.md`: earlier gap analysis.
- `docs/plan.md`: prior untracked plan, left unchanged.
- `Cargo.toml`, `.github/workflows/ci.yml`: language/dependencies and verification baseline.
- `src/source.rs`, `src/resolver.rs`, `src/lockfile.rs`: import/cache and reproducibility behavior.
- `src/lifecycle.rs`, `src/state/schema.rs`: current operation rollback and migration boundaries.
- `src/materialize.rs`, `src/planner.rs`, `src/history.rs`: projection mutation, update planning, and pin/rollback paths.
- `src/doctor.rs`, `src/response.rs`, `src/mcp.rs`: diagnostics and machine semantics.
- `src/trust.rs`, `src/telemetry.rs`: current trust and telemetry scope.
- `tests/cli/update.rs`, `tests/cli/projection.rs`, `tests/support/mod.rs`: integration conventions and fixtures.

### External primary sources

- [Agent Skills specification](https://agentskills.io/specification): portable skill format; metadata version is not a complete dependency-resolution contract.
- [Vercel skills CLI reference](https://github.com/vercel-labs/skills/blob/main/docs/commands.md): imports, updates, private repositories, experimental synchronization and frozen restore.
- [OpenAI: build skills](https://developers.openai.com/codex/skills): local discovery, metadata, and distribution guidance.
- [OpenAI: skill controls](https://learn.chatgpt.com/docs/enterprise/skills): separate local, workspace, and plugin administration boundaries.
- [OpenAI: package plugins](https://developers.openai.com/plugins/build/plugins): package formats and distribution.
- [OpenAI: import and sync plugins from GitHub](https://learn.chatgpt.com/docs/enterprise/plugin-management): workspace import, revision selection, authorization, and sync.
- [Claude: use skills](https://support.claude.com/en/articles/12512180-use-skills-in-claude): upload, sharing, and organization publication.
- [Claude: provision and manage organization skills](https://support.claude.com/en/articles/13119606-provision-and-manage-skills-for-your-organization): review of fixed submissions and organization distribution.
- [Claude: use plugins](https://support.claude.com/en/articles/13837440-use-plugins-in-claude): account-managed plugins and surface differences.
- [Claude Enterprise Plugins API](https://platform.claude.com/docs/en/manage-claude/plugins-api): upload/version/promotion/access APIs, eligibility, and scanning interactions.
- [Claude Code organization plugins](https://code.claude.com/docs/en/plugins/org): managed installation and update/source policy.
- [Claude Code plugin security](https://code.claude.com/docs/en/plugins/security): instruction/code risks and runtime boundary differences.
- [Claude Code monitoring](https://code.claude.com/docs/en/monitoring-usage): telemetry configuration and activation evidence.
- [OpenCode skills](https://opencode.ai/v2/docs/skills): discovery, configured sources, and precedence.
- [OpenCode plugins](https://opencode.ai/docs/plugins/): event and tool integration interfaces.
- [Tessl team workflow](https://docs.tessl.io/introduction-to-tessl/improve-your-first-skill): private publishing and team lifecycle.
- [Tessl skill evaluations](https://docs.tessl.io/improving-your-skills/evaluate-skill-quality-using-scenarios): scenario-based evaluation and regression evidence.

### Explicit changes from the earlier specification

- Private hosted distribution and a native companion are now planned; the earlier CLI-first/local-only slice is not the full intended product.
- Company observability covers authorized private skills and tenant-owned data, separately from optional public-source product telemetry.
- Organizational approval is explicit and digest-bound; local ownership/forking is not a security approval bypass.
- Product-improvement telemetry defaults are proposed to become opt-in, with deliberate migration and documentation.
- Native plugin distribution is included where required to deliver skills across account-managed surfaces, while a general harness manager remains out of scope.
- Paid company administration, evaluation, and champion learning are staged future products, not implied current features.
