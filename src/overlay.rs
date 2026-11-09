//! Overlay and detachment domain entry points.

use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    app::AppContext,
    error::AppError,
    history::{self, HistoryEventKind, HistoryLedger},
    lockfile::WorkspaceLockfile,
    manifest::{ImportDefinition, ManifestPath, ManifestScope, WorkspaceManifest},
    materialize::{self, PROJECTION_METADATA_FILE},
    response::AppResponse,
    skill::{
        BundledRequest, ImportResolution, ManagedImportTarget, SKILL_MANIFEST_FILE,
        classify_bundled_request,
    },
    source::{compute_effective_version_hash, current_timestamp, stored_import_root},
    state::{HistoryDetails, HistoryEntry, LocalStateStore, ManagedScope, ManagedSkillRef},
};

/// Default relative path to the overlay root.
pub const DEFAULT_OVERLAYS_DIR: &str = ".agents/overlays";
/// Hash token used when no overlay files are present.
pub(crate) const NO_OVERLAY_HASH: &str = "sha256:none";

/// Typed request for `skillctl override`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverrideRequest {
    /// Managed skill name.
    pub skill: String,
}

impl OverrideRequest {
    /// Create an override request from parsed CLI arguments.
    pub fn new(skill: String) -> Self {
        Self { skill }
    }
}

/// Typed request for `skillctl fork`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForkRequest {
    /// Managed skill name.
    pub skill: String,
}

impl ForkRequest {
    /// Create a fork request from parsed CLI arguments.
    pub fn new(skill: String) -> Self {
        Self { skill }
    }
}

/// Handle `skillctl override`.
pub fn handle_override(
    context: &AppContext,
    request: OverrideRequest,
) -> Result<AppResponse, AppError> {
    let store = LocalStateStore::open_default_for(&context.working_directory)?;
    gate_bundled_request(context, &store, &request.skill, CommandKind::Override)?;
    // Resolution runs against the documented defaults, so a name nothing
    // manages hears the same refusal `enable`, `disable`, and `remove` give it
    // instead of a raw manifest read error.
    let declared = crate::skill::load_manifest_or_default(&context.working_directory)?;
    let pinned = crate::skill::load_lockfile_or_default(&context.working_directory)?;
    let target = resolve_overlay_target(
        context,
        &declared,
        &pinned,
        &store,
        &request.skill,
        CommandKind::Override,
    )?;
    let managed_skill = target.skill;
    // A resolution that survived the defaults still writes the import
    // declaration, so the mutation needs the committed manifest.
    let manifest = WorkspaceManifest::load_required_from_workspace(context, "override")?;
    // An overlay re-pins the effective hashes in the lockfile, so a missing
    // lockfile refuses with the repairs instead of the raw read error.
    let lockfile = WorkspaceLockfile::load_required_from_workspace(context, "override")?;
    let expected = crate::apply::capture_expected_imports(context, Some(managed_skill.scope))?;
    let overlay_path = manifest
        .overrides
        .get(&request.skill)
        .cloned()
        .unwrap_or_else(|| default_overlay_path(&manifest, &request.skill));
    let overlay_root = context.working_directory.join(overlay_path.as_str());
    let plan = crate::apply::MutationPlanBuilder::new(
        crate::apply::MutationOperation::Override,
        &context.working_directory,
    )
    .skill(managed_skill.clone())
    .expect_imports(expected)
    .propose(crate::apply::ProposedImport {
        import_id: managed_skill.skill_id,
        revision: None,
        bundle_digest: lockfile
            .imports
            .get(&request.skill)
            .and_then(|entry| entry.bundle_digest().map(str::to_string)),
        bundle_policy: crate::apply::BundlePolicy::Preserve,
        summary: format!(
            "create an overlay for '{}' and recompute its effective hashes",
            request.skill
        ),
    })
    .target(manifest.path, crate::apply::TargetAction::Update)
    .target(lockfile.path, crate::apply::TargetAction::Update)
    .target(overlay_root, crate::apply::TargetAction::Create)
    .seal();

    crate::apply::run_guarded(&plan, context, |transaction| {
        transaction.track_state_database()?;
        let mut manifest = WorkspaceManifest::load_from_workspace(&context.working_directory)?;
        let mut lockfile = WorkspaceLockfile::load_from_workspace(&context.working_directory)?;
        let mut store = LocalStateStore::open_default_for(&context.working_directory)?;
        let target = resolve_overlay_target(
            context,
            &manifest,
            &lockfile,
            &store,
            &request.skill,
            CommandKind::Override,
        )?;
        let managed_skill = target.skill;
        let scope = manifest_scope(managed_skill.scope);
        let import = managed_import(&manifest, &request.skill, scope)?;
        let overlay_path = manifest
            .overrides
            .get(&import.id)
            .cloned()
            .unwrap_or_else(|| default_overlay_path(&manifest, &import.id));
        let overlay_root = context.working_directory.join(overlay_path.as_str());
        transaction.track_path(&overlay_root)?;
        let lockfile_entry =
            lockfile
                .imports
                .get_mut(&import.id)
                .ok_or_else(|| AppError::ResolutionValidation {
                    message: format!(
                        "managed import '{}' is missing from the lockfile",
                        import.id
                    ),
                })?;
        let mut install_record = store.install_record(&managed_skill)?.ok_or_else(|| {
            AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' does not have an installed state record",
                    request.skill
                ),
            }
        })?;
        let mut pin_record = store.pin_record(&managed_skill)?;
        let stored_skill_root =
            stored_import_root(managed_skill.scope, &context.working_directory, &import.id)?
                .join(lockfile_entry.source.subpath.as_str());
        let source_manifest = stored_skill_root.join(SKILL_MANIFEST_FILE);
        ensure_file(&source_manifest, "inspect stored imported skill manifest")?;

        let overlay_manifest = overlay_root.join(SKILL_MANIFEST_FILE);
        let overlay_manifest_display =
            join_relative_path(overlay_path.as_str(), SKILL_MANIFEST_FILE);

        let mut created = Vec::new();
        let mut skipped = Vec::new();
        let overlay_root_created = ensure_directory(&overlay_root, "overlay directory")?;
        record_path_action(
            overlay_root_created,
            overlay_path.as_str(),
            &mut created,
            &mut skipped,
        );
        let overlay_manifest_created =
            copy_if_missing(&source_manifest, &overlay_manifest, "overlay manifest")?;
        record_path_action(
            overlay_manifest_created,
            &overlay_manifest_display,
            &mut created,
            &mut skipped,
        );

        // Every consumer that resolves the workspace refuses an overlay file
        // without a matching file in the stored import, and that refusal
        // aborts resolution for every skill. The pinning command enforces the
        // same rule before it writes any hash, so it cannot pin content the
        // next `sync` or `update` rejects.
        validate_overlay_mapping(&overlay_root, &stored_skill_root, &import.id)?;

        let manifest_updated = match manifest.overrides.get(&import.id) {
            Some(existing) if existing == &overlay_path => false,
            _ => {
                manifest
                    .overrides
                    .insert(import.id.clone(), overlay_path.clone());
                manifest.write_to_path()?;
                true
            }
        };

        let timestamp = current_timestamp();
        let overlay_hash = hash_overlay_root(&overlay_root)?;
        let effective_version_hash = compute_effective_version_hash(
            &lockfile_entry.revision.resolved,
            &lockfile_entry.hashes.content,
            &overlay_hash,
        );
        let previous_overlay_hash = lockfile_entry.hashes.overlay.clone();

        lockfile_entry.hashes.overlay = overlay_hash.clone();
        lockfile_entry.hashes.effective_version = effective_version_hash.clone();
        lockfile.write_to_path()?;

        install_record.overlay_hash = overlay_hash.clone();
        install_record.effective_version_hash = effective_version_hash.clone();
        install_record.updated_at = timestamp.clone();
        store.upsert_install_record(&install_record)?;

        if let Some(pin_record) = pin_record.as_mut() {
            pin_record.effective_version_hash = Some(effective_version_hash.clone());
            store.upsert_pin_record(pin_record)?;
        }

        // The hashes are rewritten on every run, so the creation gate alone
        // would leave a re-pin that moves the pinned overlay digest without a
        // ledger entry. Every sibling mutation records its change, so a moved
        // pin records an overlay event of its own.
        let overlay_re_pinned = previous_overlay_hash != overlay_hash;
        if overlay_root_created || overlay_manifest_created || manifest_updated {
            let mut ledger = HistoryLedger::new(&mut store);
            ledger.record_overlay_created(&managed_skill, overlay_path.as_str(), &timestamp)?;
        } else if overlay_re_pinned {
            store.append_history_entry(&HistoryEntry {
                id: None,
                kind: HistoryEventKind::OverlayCreated,
                scope: Some(managed_skill.scope),
                skill_id: Some(managed_skill.skill_id.clone()),
                target: None,
                occurred_at: timestamp,
                summary: format!("Re-pinned overlay for {}", managed_skill.skill_id),
                details: HistoryDetails::from([
                    ("overlay_root".to_string(), json!(overlay_path.as_str())),
                    ("overlay_hash".to_string(), json!(overlay_hash)),
                ]),
            })?;
        }
        transaction.checkpoint("after-state")?;

        let summary = if overlay_root_created || overlay_manifest_created || manifest_updated {
            format!(
                "Created overlay for {} at {}",
                request.skill,
                overlay_path.as_str()
            )
        } else {
            format!(
                "Overlay for {} is ready at {}",
                request.skill,
                overlay_path.as_str()
            )
        };

        Ok(AppResponse::success("override")
            .with_summary(summary)
            .with_data(json!({
                "skill": request.skill,
                "scope": managed_skill.scope.as_str(),
                "overlay_root": overlay_path.as_str(),
                "overlay_hash": overlay_hash,
                "effective_version_hash": effective_version_hash,
                "created": created,
                "skipped": skipped,
                "manifest_updated": manifest_updated,
            })))
    })
}

/// Handle `skillctl fork`.
pub fn handle_fork(context: &AppContext, request: ForkRequest) -> Result<AppResponse, AppError> {
    run_fork(context, request).map(|(response, _)| response)
}

/// Run one fork and report the edits its preservation moved or replaced.
///
/// Both fork entries — `skillctl fork <skill>` and `update --apply --strategy
/// fork` — share this path, so the conflict-aware edit preservation, the
/// deletion refusal, the overlay writes, and the manifest release all commit
/// in one guarded transaction. A fork that fails rolls the overlay writes back
/// with the transaction, so no declared overlay survives to dead-end workspace
/// resolution.
pub(crate) fn run_fork(
    context: &AppContext,
    request: ForkRequest,
) -> Result<(AppResponse, crate::apply::ForkEditPreservation), AppError> {
    let store = LocalStateStore::open_default_for(&context.working_directory)?;
    gate_bundled_request(context, &store, &request.skill, CommandKind::Fork)?;
    // Resolution runs against the documented defaults for the same reason as
    // `override`: a name nothing manages hears the resolution refusal.
    let declared = crate::skill::load_manifest_or_default(&context.working_directory)?;
    let pinned = crate::skill::load_lockfile_or_default(&context.working_directory)?;
    let target = resolve_overlay_target(
        context,
        &declared,
        &pinned,
        &store,
        &request.skill,
        CommandKind::Fork,
    )?;
    let managed_skill = target.skill;
    // A fork removes the import declaration and its pin, so the mutation needs
    // the committed manifest.
    let manifest = WorkspaceManifest::load_required_from_workspace(context, "fork")?;
    // A fork drops the pin from the lockfile, so a missing lockfile refuses
    // with the repairs instead of the raw read error.
    let lockfile = WorkspaceLockfile::load_required_from_workspace(context, "fork")?;
    let expected = crate::apply::capture_expected_imports(context, Some(managed_skill.scope))?;
    let local_root_relative = format!(
        "{}/{}",
        manifest.layout.skills_dir.as_str().trim_end_matches('/'),
        request.skill
    );
    let local_root = context.working_directory.join(&local_root_relative);
    let scoped_context = history::context_for_scope(context, managed_skill.scope);
    let plan = crate::apply::MutationPlanBuilder::new(
        crate::apply::MutationOperation::Fork,
        &context.working_directory,
    )
    .skill(managed_skill.clone())
    .expect_imports(expected)
    .propose(crate::apply::ProposedImport {
        import_id: managed_skill.skill_id,
        revision: None,
        bundle_digest: lockfile
            .imports
            .get(&request.skill)
            .and_then(|entry| entry.bundle_digest().map(str::to_string)),
        bundle_policy: crate::apply::BundlePolicy::Release,
        summary: format!(
            "fork '{}' into local canonical ownership at '{local_root_relative}'",
            request.skill
        ),
    })
    .retained_artifacts(
        lockfile
            .imports
            .get(&request.skill)
            .and_then(|entry| entry.bundle_digest().map(str::to_string))
            .into_iter()
            .collect(),
    )
    .targets(
        materialize::sync_mutation_root_paths(
            &scoped_context,
            &manifest,
            crate::adapter::TargetScope::Workspace,
        )?,
        crate::apply::TargetAction::Replace,
    )
    .target(manifest.path, crate::apply::TargetAction::Update)
    .target(lockfile.path, crate::apply::TargetAction::Update)
    .target(local_root, crate::apply::TargetAction::Create)
    .seal();

    crate::apply::run_guarded(&plan, context, |transaction| {
        transaction.track_state_database()?;
        let mut manifest = WorkspaceManifest::load_from_workspace(&context.working_directory)?;
        let mut lockfile = WorkspaceLockfile::load_from_workspace(&context.working_directory)?;
        let mut store = LocalStateStore::open_default_for(&context.working_directory)?;
        let target = resolve_overlay_target(
            context,
            &manifest,
            &lockfile,
            &store,
            &request.skill,
            CommandKind::Fork,
        )?;
        let managed_skill = target.skill;
        if managed_skill.scope != ManagedScope::Workspace {
            return Err(AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' can only be forked from workspace scope",
                    request.skill
                ),
            });
        }

        let mut install_record = store.install_record(&managed_skill)?.ok_or_else(|| {
            AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' does not have an installed state record",
                    request.skill
                ),
            }
        })?;
        if install_record.detached || install_record.forked {
            return Err(AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' is already detached from upstream lifecycle management",
                    request.skill
                ),
            });
        }

        let scoped_context = history::context_for_scope(context, managed_skill.scope);
        for root in materialize::sync_mutation_root_paths(
            &scoped_context,
            &manifest,
            crate::adapter::TargetScope::Workspace,
        )? {
            transaction.track_path(root)?;
        }

        let import_index = manifest
            .imports
            .iter()
            .position(|import| {
                import.id == request.skill && import.scope == ManifestScope::Workspace
            })
            .ok_or_else(|| AppError::ResolutionValidation {
                message: format!(
                    "skill '{}' is not a managed import in the workspace manifest",
                    request.skill
                ),
            })?;
        let import = manifest.imports[import_index].clone();
        let lockfile_entry = lockfile.imports.get(&import.id).cloned().ok_or_else(|| {
            AppError::ResolutionValidation {
                message: format!(
                    "managed import '{}' is missing from the lockfile",
                    import.id
                ),
            }
        })?;

        let stored_skill_root =
            stored_import_root(managed_skill.scope, &context.working_directory, &import.id)?
                .join(lockfile_entry.source.subpath.as_str());
        ensure_directory_path(&stored_skill_root, "inspect stored imported skill")?;

        let local_root_relative = ManifestPath::new(local_root_relative.clone());
        let local_root = context.working_directory.join(local_root_relative.as_str());
        transaction.track_path(&local_root)?;
        // The fork rebuilds every generated copy from the stored import, so the
        // unrecorded edits of those copies move into the overlay first, inside
        // this transaction. The overlay then flows into the local canonical
        // copy, which keeps the edits instead of dropping them.
        let preservation = crate::apply::preserve_edits_for_fork(
            context,
            transaction,
            &mut manifest,
            &managed_skill,
        )?;
        prepare_local_root(&local_root, &request.skill)?;
        copy_tree_contents(&stored_skill_root, &local_root)?;

        if let Some(overlay_path) = manifest.overrides.remove(&import.id) {
            let overlay_root = context.working_directory.join(overlay_path.as_str());
            transaction.track_path(&overlay_root)?;
            match fs::metadata(&overlay_root) {
                Ok(metadata) if metadata.is_dir() => {
                    copy_tree_contents(&overlay_root, &local_root)?;
                    fs::remove_dir_all(&overlay_root).map_err(|source| {
                        AppError::FilesystemOperation {
                            action: "remove detached overlay root",
                            path: overlay_root,
                            source,
                        }
                    })?;
                }
                Ok(_) => {
                    return Err(AppError::PathConflict {
                        path: overlay_root,
                        expected: "directory",
                    });
                }
                Err(source) if source.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(AppError::FilesystemOperation {
                        action: "inspect detached overlay root",
                        path: overlay_root,
                        source,
                    });
                }
            }
        }
        remove_projection_metadata(&local_root)?;

        manifest.imports.remove(import_index);
        manifest.write_to_path()?;
        lockfile.imports.remove(&import.id);
        lockfile.write_to_path()?;

        let timestamp = current_timestamp();
        install_record.detached = true;
        install_record.forked = true;
        install_record.updated_at = timestamp.clone();

        let sync_report = materialize::sync_workspace(&scoped_context)?;

        {
            let mut ledger = HistoryLedger::new(&mut store);
            ledger.record_fork(&install_record, local_root_relative.as_str())?;
        }
        materialize::refresh_projection_state_for_scope(
            &mut store,
            context,
            managed_skill.scope,
            &sync_report,
            &timestamp,
            |_| Ok(()),
        )?;
        transaction.checkpoint("after-state")?;

        Ok((
            AppResponse::success("fork")
                .with_summary(format!(
                    "Forked {} into {}",
                    request.skill,
                    local_root_relative.as_str()
                ))
                .with_data(json!({
                    "skill": request.skill,
                    "scope": managed_skill.scope.as_str(),
                    "local_root": local_root_relative.as_str(),
                    "preserved_edits": &preservation.preserved,
                    "replaced_edits": &preservation.replaced,
                    "projection": sync_report,
                })),
            preservation,
        ))
    })
}

/// The overlay workflow command a bundled-name refusal names.
#[derive(Clone, Copy)]
enum CommandKind {
    /// `skillctl override`.
    Override,
    /// `skillctl fork`.
    Fork,
}

impl CommandKind {
    /// Command name the refusal repeats.
    fn name(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::Fork => "fork",
        }
    }

    /// What the command does to a managed import.
    fn action(self) -> &'static str {
        match self {
            Self::Override => "overlay",
            Self::Fork => "fork",
        }
    }
}

/// Refuse a request that names the bundled user-scope skill.
///
/// Override and fork act on managed imports, and the bundled skill is locally
/// owned. The lockfile pins imports by name without a scope, so without this
/// gate a checkout that also declares an import under the bundled skill's name
/// resolves the request to the bundled install record and pairs it with that
/// import's lockfile entry, which the guarded plan can never verify again.
fn gate_bundled_request(
    context: &AppContext,
    store: &LocalStateStore,
    skill: &str,
    command: CommandKind,
) -> Result<(), AppError> {
    match classify_bundled_request(context, store, skill, context.selector.scope)? {
        BundledRequest::Bundled => Err(bundled_skill_error(command, skill)),
        BundledRequest::Ambiguous => Err(bundled_name_collision_error(command, skill)),
        BundledRequest::Managed => Ok(()),
    }
}

/// The refusal a request for the bundled skill itself gets.
///
/// No overlay or fork workflow applies to the bundled skill, so the refusal
/// names the exits: stop projecting it, or manage a copy of the content as a
/// workspace import those commands can act on.
fn bundled_skill_error(command: CommandKind, skill: &str) -> AppError {
    AppError::ResolutionValidation {
        message: format!(
            "skill '{skill}' is the bundled user-scope skill, which {} cannot act on because \
             it is not a managed import; run 'skillctl --scope user remove {skill}' to stop \
             projecting it, or 'skillctl install <source> --name {skill} --scope workspace' to \
             manage a copy you can {}",
            command.name(),
            command.action(),
        ),
    }
}

/// The refusal an unscoped request gets when a declared workspace import shares
/// the bundled skill's name.
///
/// The name is ambiguous, so the refusal keeps the shared scope-resolution
/// sentence and adds which side of the collision the command can act on.
fn bundled_name_collision_error(command: CommandKind, skill: &str) -> AppError {
    AppError::ResolutionValidation {
        message: format!(
            "skill '{skill}' exists in multiple scopes; re-run with --scope: the bundled \
             user-scope skill is not a managed import, so {} cannot act on it; run 'skillctl \
             --scope workspace {} {skill}' to act on the declared workspace import",
            command.name(),
            command.name(),
        ),
    }
}

/// Resolve the managed import an override or a fork acts on.
///
/// An install record is the normal source of truth. A checkout that holds the
/// committed manifest and lockfile but no record, such as a fresh clone before
/// its first restore, still resolves the import those two files declare, so a
/// request that names that import fails on the state that actually blocks the
/// command instead of a claim that the checkout does not manage it. A caller
/// may pass the documented defaults instead, which is how a workspace without a
/// manifest hears the same refusal for a name nothing manages as `enable` and
/// `remove` give it.
fn resolve_overlay_target(
    context: &AppContext,
    manifest: &WorkspaceManifest,
    lockfile: &WorkspaceLockfile,
    store: &LocalStateStore,
    skill: &str,
    command: CommandKind,
) -> Result<ManagedImportTarget, AppError> {
    let target = crate::skill::resolve_managed_import(
        context,
        manifest,
        lockfile,
        store,
        skill,
        ImportResolution::Overlay,
    )?;
    let stored_skill_root = stored_skill_root(context, lockfile, &target.skill)?;
    if !stored_skill_root.is_dir() || target.install.is_none() {
        return Err(unrestored_import_error(command, skill, &stored_skill_root));
    }
    Ok(target)
}

/// The stored skill root an override or a fork reads content from.
fn stored_skill_root(
    context: &AppContext,
    lockfile: &WorkspaceLockfile,
    skill: &ManagedSkillRef,
) -> Result<PathBuf, AppError> {
    let subpath = lockfile
        .imports
        .get(&skill.skill_id)
        .map(|entry| entry.source.subpath.as_str().to_string())
        .unwrap_or_default();
    Ok(stored_import_root(skill.scope, &context.working_directory, &skill.skill_id)?.join(subpath))
}

/// The refusal an import this checkout cannot read from its store gets.
///
/// Override and fork copy content out of the stored import source and update
/// the local install record. The import cache and the state store are local to
/// each machine, so a checkout may hold one layer without the other, and a
/// missing cache also stops a checkout whose record survives it. The refusal
/// names the layer that is missing, and `skillctl restore --frozen` as the
/// repair that rebuilds both from the lockfile. A source the checkout cannot
/// reconstruct again also names `remove`, which is the only other exit.
fn unrestored_import_error(
    command: CommandKind,
    skill: &str,
    stored_skill_root: &Path,
) -> AppError {
    if stored_skill_root.is_dir() {
        return AppError::ResolutionValidation {
            message: format!(
                "import '{skill}' has a stored source at '{}' but no install record in this \
                 checkout; the state store is local to each machine, so run 'skillctl restore \
                 --frozen' before {command} can use the import",
                crate::planner::portable_path(stored_skill_root),
                command = command.name(),
            ),
        };
    }

    AppError::ResolutionValidation {
        message: format!(
            "import '{skill}' has no stored source in '{}'. The import cache is local to each \
             checkout, so reconstruct it with 'skillctl restore --frozen', or clear the import \
             with 'skillctl remove {skill}', before {command} can use it",
            crate::planner::portable_path(stored_skill_root),
            command = command.name(),
        ),
    }
}

fn managed_import<'a>(
    manifest: &'a WorkspaceManifest,
    skill: &str,
    scope: ManifestScope,
) -> Result<&'a ImportDefinition, AppError> {
    manifest
        .imports
        .iter()
        .find(|import| import.id == skill && import.scope == scope)
        .ok_or_else(|| AppError::ResolutionValidation {
            message: format!(
                "skill '{}' is not a managed import in the workspace manifest",
                skill
            ),
        })
}

fn default_overlay_path(manifest: &WorkspaceManifest, skill_id: &str) -> ManifestPath {
    let overlays_root = manifest.layout.overlays_dir.as_str().trim_end_matches('/');
    ManifestPath::new(format!("{overlays_root}/{skill_id}"))
}

fn manifest_scope(scope: ManagedScope) -> ManifestScope {
    match scope {
        ManagedScope::Workspace => ManifestScope::Workspace,
        ManagedScope::User => ManifestScope::User,
    }
}

fn ensure_directory(path: &Path, expected: &'static str) -> Result<bool, AppError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(false),
        Ok(_) => Err(AppError::PathConflict {
            path: path.to_path_buf(),
            expected,
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|source| AppError::FilesystemOperation {
                action: "create overlay directory",
                path: path.to_path_buf(),
                source,
            })?;
            Ok(true)
        }
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect overlay directory",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn ensure_file(path: &Path, action: &'static str) -> Result<(), AppError> {
    let metadata = fs::metadata(path).map_err(|source| AppError::FilesystemOperation {
        action,
        path: path.to_path_buf(),
        source,
    })?;

    if metadata.is_file() {
        Ok(())
    } else {
        Err(AppError::PathConflict {
            path: path.to_path_buf(),
            expected: "file",
        })
    }
}

fn copy_if_missing(
    source: &Path,
    destination: &Path,
    action: &'static str,
) -> Result<bool, AppError> {
    match fs::metadata(destination) {
        Ok(metadata) if metadata.is_file() => Ok(false),
        Ok(_) => Err(AppError::PathConflict {
            path: destination.to_path_buf(),
            expected: "file",
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
                    action: "create overlay file parent directory",
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            fs::copy(source, destination).map_err(|source| AppError::FilesystemOperation {
                action,
                path: destination.to_path_buf(),
                source,
            })?;
            Ok(true)
        }
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect overlay file",
            path: destination.to_path_buf(),
            source,
        }),
    }
}

fn record_path_action(
    created_path: bool,
    path: &str,
    created: &mut Vec<String>,
    skipped: &mut Vec<String>,
) {
    if created_path {
        created.push(path.to_string());
    } else {
        skipped.push(path.to_string());
    }
}

fn join_relative_path(root: &str, child: &str) -> String {
    format!("{}/{}", root.trim_end_matches('/'), child)
}

/// Refuse an overlay file the resolver cannot map onto the stored import.
///
/// Resolution applies every overlay file the manifest maps, and a file without
/// a counterpart in the stored import aborts workspace resolution for every
/// skill. Doctor reports the same state as `invalid-overlay-mapping`, so the
/// pinning command refuses it before any hash records it.
fn validate_overlay_mapping(
    overlay_root: &Path,
    stored_skill_root: &Path,
    import_id: &str,
) -> Result<(), AppError> {
    let mut files = Vec::new();
    collect_overlay_files(overlay_root, overlay_root, &mut files)?;
    for relative_path in files {
        let mapped = stored_skill_root.join(&relative_path);
        if !mapped.is_file() {
            return Err(AppError::ResolutionValidation {
                message: format!(
                    "overlay '{}' file '{}' does not map to an imported file of '{}'; remove \
                     the unmatched overlay file or add the file upstream before overriding it",
                    crate::planner::portable_path(overlay_root),
                    crate::planner::portable_path(&relative_path),
                    import_id,
                ),
            });
        }
    }

    Ok(())
}

fn prepare_local_root(path: &Path, skill: &str) -> Result<(), AppError> {
    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_dir() => Err(AppError::PathConflict {
            path: path.to_path_buf(),
            expected: "directory",
        }),
        Ok(_) if materialize::is_skillctl_generated_directory(path)? => {
            fs::remove_dir_all(path).map_err(|source| AppError::FilesystemOperation {
                action: "remove generated workspace skill projection",
                path: path.to_path_buf(),
                source,
            })?;
            fs::create_dir_all(path).map_err(|source| AppError::FilesystemOperation {
                action: "recreate canonical local skill root",
                path: path.to_path_buf(),
                source,
            })
        }
        Ok(_) => Err(AppError::ResolutionValidation {
            message: format!(
                "refusing to overwrite hand-authored canonical skill directory '{}' for '{}'",
                crate::planner::portable_path(path),
                skill,
            ),
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path)
            .map_err(|source| AppError::FilesystemOperation {
                action: "create canonical local skill root",
                path: path.to_path_buf(),
                source,
            }),
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect canonical local skill root",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn copy_tree_contents(source_root: &Path, destination_root: &Path) -> Result<(), AppError> {
    let mut entries = fs::read_dir(source_root)
        .map_err(|source| AppError::FilesystemOperation {
            action: "read fork source directory",
            path: source_root.to_path_buf(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| AppError::FilesystemOperation {
            action: "read fork source directory entry",
            path: source_root.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let source_path = entry.path();
        let destination_path = destination_root.join(entry.file_name());
        let metadata =
            fs::symlink_metadata(&source_path).map_err(|source| AppError::FilesystemOperation {
                action: "inspect fork source entry",
                path: source_path.clone(),
                source,
            })?;

        if metadata.is_dir() {
            fs::create_dir_all(&destination_path).map_err(|source| {
                AppError::FilesystemOperation {
                    action: "create fork destination directory",
                    path: destination_path.clone(),
                    source,
                }
            })?;
            copy_tree_contents(&source_path, &destination_path)?;
            continue;
        }

        if metadata.file_type().is_symlink() {
            return Err(AppError::ResolutionValidation {
                message: format!(
                    "fork source '{}' contains unsupported symlink '{}'",
                    crate::planner::portable_path(source_root),
                    crate::planner::portable_path(&source_path)
                ),
            });
        }

        if let Some(parent) = destination_path.parent() {
            fs::create_dir_all(parent).map_err(|source| AppError::FilesystemOperation {
                action: "create fork destination parent",
                path: parent.to_path_buf(),
                source,
            })?;
        }
        fs::copy(&source_path, &destination_path).map_err(|source| {
            AppError::FilesystemOperation {
                action: "copy forked skill file",
                path: destination_path,
                source,
            }
        })?;
    }

    Ok(())
}

fn remove_projection_metadata(root: &Path) -> Result<(), AppError> {
    let metadata_path = root.join(PROJECTION_METADATA_FILE);
    match fs::remove_file(&metadata_path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(AppError::FilesystemOperation {
            action: "remove canonical local projection metadata",
            path: metadata_path,
            source,
        }),
    }
}

fn ensure_directory_path(path: &Path, action: &'static str) -> Result<(), AppError> {
    let metadata = fs::metadata(path).map_err(|source| AppError::FilesystemOperation {
        action,
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.is_dir() {
        Ok(())
    } else {
        Err(AppError::PathConflict {
            path: path.to_path_buf(),
            expected: "directory",
        })
    }
}

pub(crate) fn hash_overlay_root(root: &Path) -> Result<String, AppError> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(AppError::ResolutionValidation {
                message: format!(
                    "overlay root '{}' must not be a symlink",
                    crate::planner::portable_path(root)
                ),
            });
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(AppError::PathConflict {
                path: root.to_path_buf(),
                expected: "directory",
            });
        }
        Ok(_) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(NO_OVERLAY_HASH.to_string());
        }
        Err(source) => {
            return Err(AppError::FilesystemOperation {
                action: "inspect overlay root",
                path: root.to_path_buf(),
                source,
            });
        }
    }

    let mut files = Vec::new();
    collect_overlay_files(root, root, &mut files)?;
    if files.is_empty() {
        return Ok(NO_OVERLAY_HASH.to_string());
    }
    files.sort();

    let mut hasher = Sha256::new();
    for relative_path in files {
        let portable_path = portable_relative_path(&relative_path)?;
        hasher.update(portable_path.as_bytes());
        hasher.update(b"\0");
        hash_file(&root.join(&relative_path), &mut hasher)?;
        hasher.update(b"\0");
    }

    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn collect_overlay_files(
    root: &Path,
    current: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), AppError> {
    let mut entries = fs::read_dir(current)
        .map_err(|source| AppError::FilesystemOperation {
            action: "read overlay directory",
            path: current.to_path_buf(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| AppError::FilesystemOperation {
            action: "read overlay directory entry",
            path: current.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(std::fs::DirEntry::path);

    for entry in entries {
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| AppError::FilesystemOperation {
                action: "inspect overlay path",
                path: path.clone(),
                source,
            })?;

        if metadata.file_type().is_symlink() {
            return Err(AppError::ResolutionValidation {
                message: format!(
                    "overlay '{}' contains unsupported symlink '{}'",
                    crate::planner::portable_path(root),
                    crate::planner::portable_path(&path)
                ),
            });
        }

        if metadata.is_dir() {
            collect_overlay_files(root, &path, files)?;
        } else if metadata.is_file() {
            files.push(
                path.strip_prefix(root)
                    .map_err(|_| AppError::ResolutionValidation {
                        message: format!(
                            "overlay path '{}' escaped the overlay root '{}'",
                            crate::planner::portable_path(&path),
                            crate::planner::portable_path(root)
                        ),
                    })?
                    .to_path_buf(),
            );
        } else {
            return Err(AppError::ResolutionValidation {
                message: format!(
                    "overlay '{}' contains a non-file, non-directory entry '{}'",
                    crate::planner::portable_path(root),
                    crate::planner::portable_path(&path)
                ),
            });
        }
    }

    Ok(())
}

fn portable_relative_path(path: &Path) -> Result<String, AppError> {
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => {
                let segment = segment
                    .to_str()
                    .ok_or_else(|| AppError::ResolutionValidation {
                        message: format!("overlay path '{}' is not valid UTF-8", path.display()),
                    })?;
                segments.push(segment);
            }
            _ => {
                return Err(AppError::ResolutionValidation {
                    message: format!(
                        "overlay path '{}' must remain relative and portable",
                        path.display()
                    ),
                });
            }
        }
    }

    Ok(segments.join("/"))
}

fn hash_file(path: &Path, hasher: &mut Sha256) -> Result<(), AppError> {
    let mut file = File::open(path).map_err(|source| AppError::FilesystemOperation {
        action: "open overlay file for hashing",
        path: path.to_path_buf(),
        source,
    })?;
    let mut buffer = [0_u8; 8192];

    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| AppError::FilesystemOperation {
                action: "read overlay file for hashing",
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[cfg(unix)]
    #[test]
    fn hash_overlay_root_rejects_symlinked_roots() {
        let temp = tempdir().expect("tempdir exists");
        let source_root = temp.path().join("source-overlay");
        let link_root = temp.path().join("overlay-link");
        fs::create_dir_all(&source_root).expect("source root exists");
        fs::write(source_root.join("SKILL.md"), "# Overlay\n").expect("overlay manifest exists");
        symlink(&source_root, &link_root).expect("symlinked root exists");

        let error = hash_overlay_root(&link_root).expect_err("symlinked roots must be rejected");

        assert!(
            error.to_string().contains("must not be a symlink"),
            "unexpected error: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn hash_overlay_root_rejects_symlinked_entries() {
        let temp = tempdir().expect("tempdir exists");
        let overlay_root = temp.path().join("overlay");
        let external_file = temp.path().join("external.md");
        fs::create_dir_all(&overlay_root).expect("overlay root exists");
        fs::write(&external_file, "# External\n").expect("external file exists");
        symlink(&external_file, overlay_root.join("SKILL.md")).expect("symlinked entry exists");

        let error =
            hash_overlay_root(&overlay_root).expect_err("symlinked overlay entries must fail");

        assert!(
            error.to_string().contains("unsupported symlink"),
            "unexpected error: {error}"
        );
    }
}
