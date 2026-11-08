//! CLI definitions for `skillctl`.
//!
//! The command surface is defined here so parsing stays strongly typed and can
//! grow independently from command execution logic.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

const ROOT_LONG_ABOUT: &str = "\
skillctl manages the local lifecycle of open SKILL.md skills across multiple agent runtimes.

Model:
  Canonical workspace skills live in .agents/skills.
  Imported sources are pinned in .agents/skillctl.lock, cached in ~/.skillctl/store/imports,
  and retained as immutable bundles in ~/.skillctl/store/artifacts.
  Run 'skillctl restore --frozen' to reconstruct a committed lockfile in a fresh checkout.
  Optional overlays live in .agents/overlays/<skill>.
  Effective skills are projected as generated runtime-visible copies.
  Local history and telemetry consent live in ~/.skillctl/state.db.

Use 'skillctl doctor' to diagnose missing, stale, shadowed, detached, or conflicting skills.";

const ROOT_AFTER_LONG_HELP: &str = "\
Quickstart:
  skillctl init
  skillctl sync
  skillctl install ../shared-skills --interactive
  skillctl explain release-notes
  skillctl doctor

Troubleshooting shortcuts:
  skillctl explain <skill> [--target <runtime>]
  skillctl path <skill>
  skillctl history [skill]
  skillctl doctor

Supported runtimes:
  codex, claude-code, github-copilot, gemini-cli, amp, opencode

Status:
  'tui' opens a read-only dashboard over the same state and inspection model as the CLI.
  Opening it does not bootstrap bundled skills or write new history entries.
  'mcp serve' exposes the same lifecycle operations to agents over MCP.";

const INIT_LONG_ABOUT: &str = "\
Create the default .agents workspace layout for skillctl.

This bootstraps:
  .agents/skills
  .agents/overlays
  .agents/skillctl.yaml
  .agents/skillctl.lock

By default skillctl also adds generated runtime roots to .git/info/exclude instead of mutating .gitignore.";

const INIT_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl init
  skillctl --cwd path/to/repo init

Next steps:
  skillctl sync
  skillctl install ../shared-skills --interactive";

const LIST_LONG_ABOUT: &str = "\
List managed installed skills from the local state store.

The result includes scope, source, pinned revision, projection roots, drift counters, and whether the skill is detached, forked, or overlay-managed.";

const LIST_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl list
  skillctl list --json
  skillctl --scope user list

Output:
  the default human output lists one block per managed skill with its scope, source, revision, projected targets, and notable state.
  --quiet suppresses human output, so only --json returns the full payload.";

const DISCOVER_LONG_ABOUT: &str = "\
Read the Agent Skills Discovery index a site publishes at /.well-known/agent-skills/index.json.

discover fetches that index, validates the declared schema version and every entry, resolves each artifact URL against the index URL, and reports the result. It is read-only: it installs nothing and changes no local state.";

const DISCOVER_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl discover https://example.com
  skillctl discover https://example.com/.well-known/agent-skills/index.json
  skillctl discover https://example.com --json

Notes:
  An origin argument resolves to /.well-known/agent-skills/index.json.
  An argument with a path is used as the index URL directly.
  An argument without a scheme is treated as an HTTPS origin.
  Entries with an unknown artifact type are skipped with a warning.
  Artifact digests are reported as declared; they are verified when the artifact is fetched.
  The origin trust policy gates every read: a loopback origin serves a local index, an HTTPS origin that is not a verified publisher warns, and a plain-HTTP origin off loopback is refused because a network attacker could serve an index whose artifacts match its declared digests.";

const INSTALL_LONG_ABOUT: &str = "\
Inspect a Git URL, local directory, local archive, or well-known discovery index for skill candidates, select exact skills, pin the installed revision, update manifest and lockfile state, and materialize projections for the chosen scope.

Interactive installs can prompt for candidate and scope selection.
Non-interactive installs never guess and require an exact selector such as --name <skill> when multiple candidates exist.";

const INSTALL_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl install ../shared-skills --interactive
  skillctl install ./skills.tar.gz --name release-notes --scope user
  skillctl i https://github.com/acme/skills.git --name ai-sdk --scope workspace
  skillctl install https://skills.example.com --name code-review --scope workspace

Notes:
  An HTTPS or HTTP URL at an origin root or ending in .json installs one entry of the well-known discovery index that URL publishes, and --name selects the entry.
  A well-known install records a 'well-known' source, the index entry it came from, and the verified artifact digest as its pinned revision.
  Imported sources are cached under ~/.skillctl/store/imports.
  Use 'skillctl path <skill>' or 'skillctl explain <skill>' to inspect the result.";

const REMOVE_LONG_ABOUT: &str = "\
Remove a managed skill from workspace or user scope.

skillctl removes managed imports, cached stored sources, and generated projections for the selected skill. It does not delete canonical local skills or overlays unless they are part of a detached workspace copy.";

const REMOVE_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl remove release-notes
  skillctl --scope user remove skillctl";

const SYNC_LONG_ABOUT: &str = "\
Recompute the effective-skill graph and materialize generated runtime-visible copies.

sync resolves canonical skills, imports, overlays, conflicts, and planned roots, then refreshes only the directories previously managed by skillctl.

A generated copy that holds edits skillctl did not record is preserved and reported. Pass --discard to back such a copy up durably and replace it.

An overlay that no longer hashes to the digest the lockfile pins is projected, but sync reports it and exits with 2, because a frozen restore refuses that pair.";

const SYNC_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl sync
  skillctl --scope workspace --target codex --target gemini-cli sync
  skillctl sync --discard

Use 'skillctl doctor' after sync if a runtime still does not load the expected skill.";

const UPDATE_LONG_ABOUT: &str = "\
Check managed imported skills against their upstream source, record update-check history, detect overlays or local drift, and recommend the next safe action.

A Git import is compared with the revision its remote names. A well-known import is compared with the digest its discovery index declares now, so a changed digest is an update and --apply downloads and verifies the artifact it names.

Without --apply, update reports plans and follow-up actions instead of overwriting local changes. With --apply it runs the shared guarded apply engine: the plan is verified against the lockfile, the bundle digest, and local state before anything changes.";

const UPDATE_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl update
  skillctl update ai-sdk --scope workspace
  skillctl update ai-sdk --json
  skillctl update ai-sdk --apply
  skillctl update ai-sdk --apply --strategy overlay
  skillctl update --plan update-plan.json
  skillctl update --apply --plan-file update-plan.json

Apply strategies (used with --apply):
  clean: apply only when no unmanaged local edits exist
  overlay: move unmanaged edits into .agents/overlays/<skill>, then apply
  discard: back unmanaged edits up durably, drop them, then apply
  fork: detach into local canonical ownership instead of applying
  skip: keep the current pin and record the decision

Trust gate:
  an unreviewed import with a top-level scripts/ directory and an available update is blocked. The apply refuses with exit code 4 and changes nothing. Record a local review with 'skillctl review <skill> --approve' to let the apply through, use '--strategy fork' to take local ownership, or '--strategy skip' to keep the current pin.

Well-known imports:
  the check re-reads the discovery index and compares the declared digest with the pin. --apply re-reads the index again, refuses when it moved past the digest the check saw, and moves the import to the artifact that digest verifies. 'pin' and 'rollback' stay Git-only, because a discovery index declares no branch or tag to pin.

Plans:
  --plan <path> writes a typed, immutable plan document without changing anything. A skill whose edits block an update still contributes a plan, and the run warns which strategy resolves them.
  --apply --plan-file <path> applies exactly that document. A plan built against different local state fails with a stale-plan error before any mutation, and a document with nothing to apply warns and exits 2.

Use 'skillctl history <skill>' and 'skillctl explain <skill>' to inspect drift.";

const RESTORE_LONG_ABOUT: &str = "\
Reconstruct every enabled import from the portable lockfile and the committed overlays.

restore rebuilds the local import cache from retained artifacts or from the exact locked revision, verifies the reconstructed content against the digests pinned in .agents/skillctl.lock, verifies each declared overlay against its pinned overlay digest, and then syncs projections. Overlay files are committable portable state next to the lockfile, so a checkout that omits them cannot reproduce a pinned overlay digest. It never advances a pinned revision: a locked revision that is unavailable upstream is a failure, not a reason to take the branch HEAD.";

const RESTORE_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl restore --frozen
  skillctl restore --frozen --offline
  skillctl restore

Modes:
  frozen: reconstruct and verify without touching the lockfile
  default: also upgrade a version 1 lockfile to the current version, recording the canonical bundle digest of already-pinned content. Pinned revisions and content digests never move.

Offline:
  --offline reconstructs only from verified cached artifacts and never contacts a source. It never pretends to have checked upstream.

Team workflow:
  1. One teammate runs skillctl install and commits .agents/skillctl.yaml, .agents/skillctl.lock, and .agents/overlays/
  2. Everyone else clones the repository and runs skillctl restore --frozen
  3. skillctl verify reports integrity without changing anything";

const VERIFY_LONG_ABOUT: &str = "\
Verify pinned dependency state without repairing it.

verify reports integrity, cache state, portability, policy eligibility, and freshness for every enabled import. It reads the manifest, lockfile, import cache, and artifact store, writes nothing, and never contacts a source.";

const VERIFY_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl verify
  skillctl verify --json

Reported per import:
  integrity: verified, mismatch, or missing against the pinned digests
  cache: workspace import store and retained artifact state
  overlay: declared overlay directory state against its pinned digest
  portability: whether the pinned content can be reconstructed here
  freshness: whether upstream checks exist for this source kind
  policy: local policy state. Digest verification proves content matches the lockfile, not that anyone reviewed it.

Exit codes:
  0 all imports verified
  2 verified with warnings, such as a missing cache that can be restored
  3 integrity failure or content that cannot be reconstructed";

const PIN_LONG_ABOUT: &str = "\
Resolve and pin an imported skill to an exact revision.

pin updates the manifest, lockfile, stored import cache, install record, and projections so later update and rollback flows have an exact baseline.

Trust gate:
  a pin moves the import to a revision, so an unreviewed import with a top-level scripts/ directory is blocked exactly like an update apply. The pin refuses with exit code 4 and changes nothing. Record a local review with 'skillctl review <skill> --approve' to let the move through, or run 'skillctl fork <skill>' to take local ownership.";

const PIN_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl pin ai-sdk main
  skillctl pin ai-sdk 0123456789abcdef";

const ROLLBACK_LONG_ABOUT: &str = "\
Re-activate a previously recorded version or commit for a managed imported skill.

rollback restores the pinned revision, updates projections, and records the transition in local history.

Trust gate:
  a rollback moves the import to a revision, so an unreviewed import with a top-level scripts/ directory is blocked exactly like an update apply. The rollback refuses with exit code 4 and changes nothing. Record a local review with 'skillctl review <skill> --approve' to let the move through, or run 'skillctl fork <skill>' to take local ownership of the current content.";

const ROLLBACK_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl rollback ai-sdk 0123456789abcdef
  skillctl rollback ai-sdk sha256:effective-version

Use 'skillctl history <skill>' to discover prior revisions and rollback points.";

const HISTORY_LONG_ABOUT: &str = "\
Show the local install and modification ledger for one skill or for the whole workspace.

History entries include installs, update checks, applied projections, pins, rollbacks, overlays, forks, cleanup, and telemetry consent changes.

A canonical local skill in .agents/skills holds no install record, so its report lists no events and names the canonical root instead. Run 'skillctl explain <skill>' to see where that skill projects.";

const HISTORY_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl history
  skillctl history ai-sdk
  skillctl history ai-sdk --json

Output:
  the default human output lists one line per event with its time, kind, skill, recorded revision, and summary.
  --quiet suppresses human output, so only --json returns the full payload.";

const DOCTOR_LONG_ABOUT: &str = "\
Diagnose missing, stale, shadowed, detached, or conflicting skills across enabled runtimes.

doctor validates manifests, lockfile state, overlays, projections, precedence roots, vendor-specific metadata, trust signals, and generated-copy drift.";

const DOCTOR_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl doctor
  skillctl doctor --target opencode
  skillctl doctor --json

Start here when:
  a runtime cannot see a skill
  a generated copy looks stale
  explain shows a conflict or shadowed winner
  update reports drift or trust warnings

Local state recovery:
  skillctl doctor --repair-state
  skillctl doctor --repair-state --apply";

const EXPLAIN_LONG_ABOUT: &str = "\
Explain which candidate wins for a projected skill name, why it won, which runtimes can see it, and whether the active copy differs from the pinned managed source.

Use explain to inspect winner selection, shadowed candidates, same-name conflicts, target roots, and detached or forked state.";

const EXPLAIN_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl explain ai-sdk
  skillctl explain ai-sdk --target codex
  skillctl explain ai-sdk --scope workspace --json

Follow up with 'skillctl path <skill>' for concrete filesystem locations.";

const INSPECT_LONG_ABOUT: &str = "\
Inspect the active skill's preflight requirements: trust state, declared capabilities, required credentials, required tools, and target visibility.

Use inspect when you want one focused readiness view before invoking or promoting a skill, instead of piecing the state together from install warnings and doctor issues.";

const INSPECT_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl inspect ai-sdk
  skillctl inspect ai-sdk --target codex
  skillctl inspect ai-sdk --scope workspace --json

Follow up with 'skillctl explain <skill>' when you need full winner-selection detail.";

const OVERRIDE_LONG_ABOUT: &str = "\
Create or reuse an overlay directory for a managed imported skill.

Overlays live under .agents/overlays/<skill> and replace matching upstream files without forking the entire imported skill.";

const OVERRIDE_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl override ai-sdk

After editing overlay files, run 'skillctl override <skill>' again to re-pin the edited digest, then 'skillctl sync' or 'skillctl update <skill>' to inspect the new effective version. Until the re-pin, sync projects the edit and exits with 2 to say the lockfile still pins the previous overlay digest.";

const FORK_LONG_ABOUT: &str = "\
Detach a managed imported workspace skill into canonical local ownership under .agents/skills.

fork copies the current effective skill, merges any overlay content into the local copy, removes managed import state for that skill, and marks the install as detached from upstream lifecycle management.";

const FORK_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl fork ai-sdk

Use this when you want full local ownership instead of keeping an upgradeable imported skill.";

const REVIEW_LONG_ABOUT: &str = "\
Show or record the local review decision for a managed imported skill.

Without a decision flag, review reports the trust state of the import and the review that is currently on record. '--approve' records that a human reviewed the effective content, which moves the import to 'imported-reviewed' and stops the trust gate from holding back update applies, pins, and rollbacks. '--revoke' withdraws a recorded review.

A review covers one effective version of the skill. When an update, a pin, or a rollback moves the import to new content, that content is unreviewed again, so the gate returns and a fresh review is needed.";

const REVIEW_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl review ai-sdk
  skillctl review ai-sdk --approve
  skillctl review ai-sdk --approve --note 'scripts inspected, safe'
  skillctl review ai-sdk --revoke

Notes:
  A review is a local decision recorded in ~/.skillctl/state.db and in local history. It is not written to .agents/skillctl.lock, so it does not travel with the repository.
  The command refuses a bundled, detached, or forked skill, because those are locally owned and need no review.";

const ENABLE_LONG_ABOUT: &str = "\
Enable a managed import so it participates in resolution and projection again.";

const ENABLE_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl enable ai-sdk";

const DISABLE_LONG_ABOUT: &str = "\
Disable a managed import without deleting its recorded history or cached source.

Disabled imports stay on disk but stop participating in the effective-skill graph until re-enabled.";

const DISABLE_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl disable ai-sdk";

const PATH_LONG_ABOUT: &str = "\
Show canonical, stored, overlay, planned-root, and projected paths for a managed skill.

Use this when you need to know exactly which directory is canonical, where the cached import lives, and which runtime roots should contain generated copies. A canonical local skill under .agents/skills/ resolves too, even when it was never installed.";

const PATH_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl path ai-sdk
  skillctl path ai-sdk --target codex --json

Output:
  the default human output lists the source, revision, canonical, stored, overlay, planned-root, and projected paths.
  --quiet suppresses human output, so only --json returns the full payload.";

const VALIDATE_LONG_ABOUT: &str = "\
Validate manifests, lockfile state, skill directories, and overlay mappings without checking runtime projection drift.

Use validate for structural correctness and use doctor when you also need runtime-facing diagnostics.";

const VALIDATE_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl validate
  skillctl validate --json";

const CLEAN_LONG_ABOUT: &str = "\
Remove only skillctl-generated projections and generated cached state that is no longer needed.

clean never deletes canonical workspace skills or overlays unless you explicitly remove the managed skill through the lifecycle commands.";

const CLEAN_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl clean
  skillctl --scope workspace clean";

const TUI_LONG_ABOUT: &str = "\
Open the terminal inspection UI.

The terminal inspection UI is a read-only dashboard for installed versions,
update availability, overlays, local modifications, target visibility, pin or
rollback context, and recent history.

Opening it does not bootstrap bundled skills or write new history entries.

Every suggested action maps to a documented CLI command.";

const TUI_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl tui
  skillctl --scope workspace --name ai-sdk tui
  skillctl --target codex tui

Mapped actions:
  refresh update state: skillctl update [skill]
  inspect visibility: skillctl explain <skill>
  inspect filesystem roots: skillctl path <skill>
  inspect history: skillctl history [skill]
  pin or roll back: skillctl pin <skill> <ref> / skillctl rollback <skill> <version-or-commit>";

const TELEMETRY_LONG_ABOUT: &str = "\
Inspect or change public-only telemetry consent.

Telemetry is enabled by default after a first-run notice. Install and update events are classified locally, and nothing is transmitted today because no emission backend exists. Local history is always kept even when telemetry is disabled.";

const TELEMETRY_AFTER_LONG_HELP: &str = "\
Examples:
  skillctl telemetry status
  skillctl telemetry disable
  skillctl telemetry enable";

const TELEMETRY_STATUS_LONG_ABOUT: &str = "\
Show the effective telemetry policy, stored consent, and whether the first-run notice has already been seen.";

const TELEMETRY_ENABLE_LONG_ABOUT: &str = "\
Enable telemetry consent for install and update events. Events are classified locally, and nothing is transmitted today because no emission backend exists.";

const TELEMETRY_DISABLE_LONG_ABOUT: &str = "\
Disable telemetry while keeping local history, pins, and diagnostics fully available.";

const MCP_LONG_ABOUT: &str = "\
Run the MCP surface for skillctl.

The MCP server exposes the same lifecycle operations and JSON envelopes as the CLI.";

const MCP_AFTER_LONG_HELP: &str = "\
Available v1 tools:
  skills_list -> skillctl list --json
  skills_discover -> skillctl discover <url> --json
  skills_install -> skillctl install <source> --json
  skills_remove -> skillctl remove <skill> --json
  skills_sync -> skillctl sync --json
  skills_restore -> skillctl restore --frozen --json
  skills_verify -> skillctl verify --json
  skills_update -> skillctl update [skill] --json
  skills_update_apply -> skillctl update [skill] --apply --json
  skills_rollback -> skillctl rollback <skill> <version-or-commit> --json
  skills_history -> skillctl history [skill] --json
  skills_explain -> skillctl explain <skill> --json
  skills_override_create -> skillctl override <skill> --json
  skills_validate -> skillctl validate --json
  skills_doctor -> skillctl doctor --json
  skills_telemetry_status -> skillctl telemetry status --json

Required arguments:
  skills_restore needs an explicit frozen boolean. frozen=true reconstructs without
  changing the lockfile. frozen=false may upgrade a legacy version 1 lockfile.";

const MCP_SERVE_LONG_ABOUT: &str = "\
Start the MCP server process.

This command serves newline-delimited JSON-RPC over stdin/stdout and exposes the v1 lifecycle tools.";

/// Top-level CLI parser for `skillctl`.
#[derive(Debug, Parser)]
#[command(
    name = "skillctl",
    version,
    about = "Local-first cross-agent skill manager for the open SKILL.md ecosystem",
    long_about = ROOT_LONG_ABOUT,
    after_long_help = ROOT_AFTER_LONG_HELP
)]
pub struct Cli {
    /// Global execution flags shared across commands.
    #[command(flatten)]
    pub global: GlobalArgs,

    /// Parsed top-level subcommand, or `None` when rendering help.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Execution flags that every command can consume.
#[derive(Clone, Debug, Default, Args)]
pub struct GlobalArgs {
    /// Emit machine-readable JSON instead of human text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Suppress non-essential human output.
    #[arg(long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Emit additional human output.
    #[arg(long, global = true, conflicts_with = "quiet")]
    pub verbose: bool,

    /// Disable interactive prompts.
    #[arg(long, global = true, conflicts_with = "interactive")]
    pub no_input: bool,

    /// Force interactive prompts when the command supports them.
    #[arg(long, global = true, conflicts_with = "no_input")]
    pub interactive: bool,

    /// Exact skill name selector for commands that need one.
    #[arg(long, global = true)]
    pub name: Option<String>,

    /// Preferred execution scope.
    #[arg(long, global = true, value_enum)]
    pub scope: Option<Scope>,

    /// Runtime target filter.
    #[arg(
        long,
        global = true,
        help = "Runtime target filter. Repeat to narrow commands to enabled manifest targets"
    )]
    pub target: Vec<String>,

    /// Working directory override for command execution.
    #[arg(
        long,
        global = true,
        help = "Working directory override used to locate the workspace manifest"
    )]
    pub cwd: Option<PathBuf>,
}

/// Supported execution scopes for runtime planning.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Scope {
    /// Workspace-local scope.
    Workspace,
    /// User-wide scope.
    User,
}

/// Parsed top-level commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize a workspace-local skillctl layout.
    #[command(long_about = INIT_LONG_ABOUT, after_long_help = INIT_AFTER_LONG_HELP)]
    Init,
    /// List managed skills.
    #[command(long_about = LIST_LONG_ABOUT, after_long_help = LIST_AFTER_LONG_HELP)]
    List,
    /// Discover skills from a well-known discovery index.
    #[command(
        long_about = DISCOVER_LONG_ABOUT,
        after_long_help = DISCOVER_AFTER_LONG_HELP
    )]
    Discover(DiscoverArgs),
    /// Install a skill from a Git URL, local path, archive, or discovery index.
    #[command(
        visible_alias = "i",
        long_about = INSTALL_LONG_ABOUT,
        after_long_help = INSTALL_AFTER_LONG_HELP
    )]
    Install(InstallArgs),
    /// Remove a managed skill.
    #[command(long_about = REMOVE_LONG_ABOUT, after_long_help = REMOVE_AFTER_LONG_HELP)]
    Remove(SkillArg),
    /// Recompute and materialize projections.
    #[command(
        visible_alias = "s",
        long_about = SYNC_LONG_ABOUT,
        after_long_help = SYNC_AFTER_LONG_HELP
    )]
    Sync(SyncArgs),
    /// Reconstruct imports from the lockfile.
    #[command(
        long_about = RESTORE_LONG_ABOUT,
        after_long_help = RESTORE_AFTER_LONG_HELP
    )]
    Restore(RestoreArgs),
    /// Verify pinned dependency state read-only.
    #[command(
        long_about = VERIFY_LONG_ABOUT,
        after_long_help = VERIFY_AFTER_LONG_HELP
    )]
    Verify,
    /// Check for skill updates and recommend next actions.
    #[command(
        visible_alias = "u",
        long_about = UPDATE_LONG_ABOUT,
        after_long_help = UPDATE_AFTER_LONG_HELP
    )]
    Update(UpdateArgs),
    /// Pin a skill to a specific revision.
    #[command(long_about = PIN_LONG_ABOUT, after_long_help = PIN_AFTER_LONG_HELP)]
    Pin(PinArgs),
    /// Roll back a skill to a previous version or commit.
    #[command(long_about = ROLLBACK_LONG_ABOUT, after_long_help = ROLLBACK_AFTER_LONG_HELP)]
    Rollback(RollbackArgs),
    /// Show version and modification history.
    #[command(
        visible_alias = "hist",
        long_about = HISTORY_LONG_ABOUT,
        after_long_help = HISTORY_AFTER_LONG_HELP
    )]
    History(OptionalSkillArg),
    /// Diagnose missing, stale, shadowed, or conflicting skills.
    #[command(
        visible_alias = "dr",
        long_about = DOCTOR_LONG_ABOUT,
        after_long_help = DOCTOR_AFTER_LONG_HELP
    )]
    Doctor(DoctorArgs),
    /// Explain a skill's winner, target visibility, and drift.
    #[command(
        visible_alias = "x",
        long_about = EXPLAIN_LONG_ABOUT,
        after_long_help = EXPLAIN_AFTER_LONG_HELP
    )]
    Explain(SkillArg),
    /// Inspect a skill's preflight trust and requirements.
    #[command(long_about = INSPECT_LONG_ABOUT, after_long_help = INSPECT_AFTER_LONG_HELP)]
    Inspect(SkillArg),
    /// Create an overlay for a managed skill.
    #[command(long_about = OVERRIDE_LONG_ABOUT, after_long_help = OVERRIDE_AFTER_LONG_HELP)]
    Override(SkillArg),
    /// Detach a managed skill into a local canonical copy.
    #[command(long_about = FORK_LONG_ABOUT, after_long_help = FORK_AFTER_LONG_HELP)]
    Fork(SkillArg),
    /// Show or record a local review decision for an import.
    #[command(long_about = REVIEW_LONG_ABOUT, after_long_help = REVIEW_AFTER_LONG_HELP)]
    Review(ReviewArgs),
    /// Enable a managed skill.
    #[command(long_about = ENABLE_LONG_ABOUT, after_long_help = ENABLE_AFTER_LONG_HELP)]
    Enable(SkillArg),
    /// Disable a managed skill.
    #[command(long_about = DISABLE_LONG_ABOUT, after_long_help = DISABLE_AFTER_LONG_HELP)]
    Disable(SkillArg),
    /// Show filesystem paths for a managed or canonical local skill.
    #[command(
        visible_alias = "pt",
        long_about = PATH_LONG_ABOUT,
        after_long_help = PATH_AFTER_LONG_HELP
    )]
    Path(SkillArg),
    /// Validate manifests, skills, and projections.
    #[command(long_about = VALIDATE_LONG_ABOUT, after_long_help = VALIDATE_AFTER_LONG_HELP)]
    Validate,
    /// Remove generated state and stale projections.
    #[command(long_about = CLEAN_LONG_ABOUT, after_long_help = CLEAN_AFTER_LONG_HELP)]
    Clean,
    /// Open the terminal UI.
    #[command(long_about = TUI_LONG_ABOUT, after_long_help = TUI_AFTER_LONG_HELP)]
    Tui,
    /// Inspect or change telemetry settings.
    #[command(long_about = TELEMETRY_LONG_ABOUT, after_long_help = TELEMETRY_AFTER_LONG_HELP)]
    Telemetry {
        /// Parsed telemetry subcommand.
        #[command(subcommand)]
        command: TelemetryCommand,
    },
    /// Run the MCP server.
    #[command(long_about = MCP_LONG_ABOUT, after_long_help = MCP_AFTER_LONG_HELP)]
    Mcp {
        /// Parsed MCP subcommand.
        #[command(subcommand)]
        command: McpCommand,
    },
}

impl Command {
    /// Return the stable command identifier used in responses.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::List => "list",
            Self::Discover(_) => "discover",
            Self::Install(_) => "install",
            Self::Remove(_) => "remove",
            Self::Sync(_) => "sync",
            Self::Restore(_) => "restore",
            Self::Verify => "verify",
            Self::Update(_) => "update",
            Self::Pin(_) => "pin",
            Self::Rollback(_) => "rollback",
            Self::History(_) => "history",
            Self::Doctor(_) => "doctor",
            Self::Explain(_) => "explain",
            Self::Inspect(_) => "inspect",
            Self::Override(_) => "override",
            Self::Fork(_) => "fork",
            Self::Review(_) => "review",
            Self::Enable(_) => "enable",
            Self::Disable(_) => "disable",
            Self::Path(_) => "path",
            Self::Validate => "validate",
            Self::Clean => "clean",
            Self::Tui => "tui",
            Self::Telemetry { command } => command.name(),
            Self::Mcp { command } => command.name(),
        }
    }
}

/// Arguments for `install`.
#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Source repository, path, or archive to install from.
    pub source: String,
}

/// Arguments for `discover`.
#[derive(Debug, Args)]
pub struct DiscoverArgs {
    /// Origin or index URL to read skills from.
    pub url: String,
}

/// Arguments for `update`.
#[derive(Clone, Debug, Default, Args)]
pub struct UpdateArgs {
    /// Optional managed skill name.
    pub skill: Option<String>,

    /// Apply the planned update through the guarded apply engine.
    #[arg(long)]
    pub apply: bool,

    /// How unmanaged local edits are treated during an apply.
    #[arg(long, requires = "apply", value_enum)]
    pub strategy: Option<UpdateStrategy>,

    /// Write a typed plan document and exit without changing anything.
    #[arg(long, value_name = "PATH", conflicts_with = "apply")]
    pub plan: Option<PathBuf>,

    /// Apply exactly this plan document instead of a freshly built plan.
    #[arg(long, requires = "apply", value_name = "PATH")]
    pub plan_file: Option<PathBuf>,
}

/// How an apply treats unmanaged local edits.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateStrategy {
    /// Apply only when no unmanaged local edits exist.
    #[default]
    Clean,
    /// Move unmanaged edits into the managed overlay, then apply.
    Overlay,
    /// Back unmanaged edits up durably, drop them, then apply.
    Discard,
    /// Detach into local canonical ownership instead of applying.
    Fork,
    /// Keep the current pin and record the decision.
    Skip,
}

impl UpdateStrategy {
    /// Return the strategy identifier used in responses and plan documents.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Overlay => "overlay",
            Self::Discard => "discard",
            Self::Fork => "fork",
            Self::Skip => "skip",
        }
    }

    /// Map the CLI value onto the shared engine strategy.
    pub const fn engine(self) -> crate::apply::ApplyStrategy {
        match self {
            Self::Clean => crate::apply::ApplyStrategy::Clean,
            Self::Overlay => crate::apply::ApplyStrategy::Overlay,
            Self::Discard => crate::apply::ApplyStrategy::Discard,
            Self::Fork => crate::apply::ApplyStrategy::Fork,
            Self::Skip => crate::apply::ApplyStrategy::Skip,
        }
    }
}

/// Arguments for `sync`.
#[derive(Clone, Copy, Debug, Default, Args)]
pub struct SyncArgs {
    /// Back edited generated copies up durably, then replace them.
    #[arg(long)]
    pub discard: bool,
}

/// Arguments for `restore`.
#[derive(Clone, Copy, Debug, Default, Args)]
pub struct RestoreArgs {
    /// Reconstruct and verify without changing the lockfile.
    #[arg(long)]
    pub frozen: bool,

    /// Use only verified cached artifacts and never contact a source.
    #[arg(long)]
    pub offline: bool,
}

/// Arguments for `doctor`.
#[derive(Clone, Copy, Debug, Default, Args)]
pub struct DoctorArgs {
    /// Preview recoverable local state instead of running diagnostics.
    ///
    /// Covers interrupted operations, stranded schema-v1 tables, and a state
    /// database SQLite cannot read.
    #[arg(long)]
    pub repair_state: bool,

    /// Apply the previewed repairs instead of only showing them.
    #[arg(long, requires = "repair_state")]
    pub apply: bool,
}

/// Arguments for commands that require a skill name.
#[derive(Debug, Args)]
pub struct SkillArg {
    /// Managed skill name.
    pub skill: String,
}

/// Arguments for commands that optionally target a single skill.
#[derive(Debug, Args)]
pub struct OptionalSkillArg {
    /// Optional managed skill name.
    pub skill: Option<String>,
}

/// Arguments for `review`.
#[derive(Debug, Args)]
pub struct ReviewArgs {
    /// Managed skill name.
    pub skill: String,

    /// Record that the effective content was reviewed.
    #[arg(long, conflicts_with = "revoke")]
    pub approve: bool,

    /// Withdraw the recorded review decision.
    #[arg(long)]
    pub revoke: bool,

    /// Note stored with the decision.
    #[arg(long, requires = "approve", value_name = "TEXT")]
    pub note: Option<String>,
}

/// Arguments for `pin`.
#[derive(Debug, Args)]
pub struct PinArgs {
    /// Managed skill name.
    pub skill: String,
    /// Exact revision to pin.
    #[arg(name = "ref")]
    pub ref_spec: String,
}

/// Arguments for `rollback`.
#[derive(Debug, Args)]
pub struct RollbackArgs {
    /// Managed skill name.
    pub skill: String,
    /// Previous version identifier or pinned commit.
    pub version_or_commit: String,
}

/// Subcommands for telemetry management.
#[derive(Debug, Subcommand)]
pub enum TelemetryCommand {
    /// Show current telemetry settings.
    #[command(long_about = TELEMETRY_STATUS_LONG_ABOUT)]
    Status,
    /// Enable telemetry.
    #[command(long_about = TELEMETRY_ENABLE_LONG_ABOUT)]
    Enable,
    /// Disable telemetry.
    #[command(long_about = TELEMETRY_DISABLE_LONG_ABOUT)]
    Disable,
}

impl TelemetryCommand {
    /// Return the stable command identifier used in responses.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Status => "telemetry-status",
            Self::Enable => "telemetry-enable",
            Self::Disable => "telemetry-disable",
        }
    }
}

/// Subcommands for the MCP surface.
#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// Start the MCP server.
    #[command(long_about = MCP_SERVE_LONG_ABOUT)]
    Serve,
}

impl McpCommand {
    /// Return the stable command identifier used in responses.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Serve => "mcp-serve",
        }
    }
}
