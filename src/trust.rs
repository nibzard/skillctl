//! Trust-state modeling for imported and local skills.

use std::{fs, io, path::Path};

use serde::Serialize;

use crate::{error::AppError, resolver::ResolvedSkillCandidate, state::InstallRecord};

/// Stable trust states for managed and effective skills.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustState {
    /// Canonical local ownership or bundled content.
    LocalTrusted,
    /// Imported content that has not been reviewed yet.
    ImportedUnreviewed,
    /// Imported content that has been explicitly reviewed.
    ImportedReviewed,
}

/// User-visible risk level derived from one trust decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustRiskLevel {
    /// No elevated trust warning applies.
    Normal,
    /// Imported script-bearing content remains unreviewed.
    Elevated,
}

/// Operation currently blocked by a trust gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustBlockedAction {
    /// Applying an upstream update is blocked until review or fork.
    ApplyUpdate,
}

/// Structured trust decision surfaced in JSON and local history.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillTrust {
    /// Trust state of the stored source.
    pub source_state: TrustState,
    /// Trust state of the currently effective skill.
    pub effective_state: TrustState,
    /// Derived risk level.
    pub risk_level: TrustRiskLevel,
    /// Whether the effective skill includes a top-level `scripts/` tree.
    pub contains_scripts: bool,
    /// Whether a human review is still required.
    pub review_required: bool,
    /// Current operation-level trust-gate blocks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_actions: Vec<TrustBlockedAction>,
    /// Deterministic warnings relevant to the current operation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl SkillTrust {
    /// Build trust for canonical local or bundled content.
    pub fn local(contains_scripts: bool) -> Self {
        Self {
            source_state: TrustState::LocalTrusted,
            effective_state: TrustState::LocalTrusted,
            risk_level: TrustRiskLevel::Normal,
            contains_scripts,
            review_required: false,
            blocked_actions: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Build trust for an imported skill before any explicit review workflow exists.
    pub fn imported_unreviewed(skill_name: &str, contains_scripts: bool) -> Self {
        let mut warnings = Vec::new();
        let risk_level = if contains_scripts {
            warnings.push(format!(
                "imported skill '{}' contains scripts and remains unreviewed",
                skill_name
            ));
            TrustRiskLevel::Elevated
        } else {
            TrustRiskLevel::Normal
        };

        Self {
            source_state: TrustState::ImportedUnreviewed,
            effective_state: TrustState::ImportedUnreviewed,
            risk_level,
            contains_scripts,
            review_required: true,
            blocked_actions: Vec::new(),
            warnings,
        }
    }

    /// Return whether this trust decision should emit warnings.
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    /// Add a trust-gate block for update application when risk is elevated.
    pub fn block_apply_update(mut self, skill_name: &str) -> Self {
        if self.risk_level == TrustRiskLevel::Elevated {
            self.blocked_actions.push(TrustBlockedAction::ApplyUpdate);
            self.warnings.push(format!(
                "trust gate is blocking update apply for '{}' until it is reviewed or forked",
                skill_name
            ));
        }
        self
    }
}

/// Build trust for one resolved effective-skill candidate.
pub fn trust_for_candidate(candidate: &ResolvedSkillCandidate) -> SkillTrust {
    let contains_scripts = candidate_contains_scripts(candidate);
    if candidate.import.is_some() {
        SkillTrust::imported_unreviewed(candidate.skill.name.as_str(), contains_scripts)
    } else {
        SkillTrust::local(contains_scripts)
    }
}

/// Build trust for one current install record using a known effective file root.
pub fn trust_for_install_record(
    install: &InstallRecord,
    builtin: bool,
    effective_root: Option<&Path>,
) -> Result<SkillTrust, AppError> {
    let contains_scripts = match effective_root {
        Some(root) => directory_contains_scripts(root)?,
        None => false,
    };

    if builtin || install.source_url.starts_with("builtin://") || install.detached || install.forked
    {
        return Ok(SkillTrust::local(contains_scripts));
    }

    Ok(SkillTrust::imported_unreviewed(
        install.skill.skill_id.as_str(),
        contains_scripts,
    ))
}

/// Return whether a resolved candidate includes files under `scripts/`.
pub fn candidate_contains_scripts(candidate: &ResolvedSkillCandidate) -> bool {
    candidate
        .files
        .iter()
        .any(|file| path_is_scripts_path(&file.relative_path))
}

/// Return whether a relative path is inside the top-level `scripts/` directory.
pub fn path_is_scripts_path(path: &Path) -> bool {
    path.components()
        .next()
        .is_some_and(|component| component.as_os_str() == "scripts")
}

/// Return whether a skill root contains a top-level `scripts/` directory.
pub fn directory_contains_scripts(root: &Path) -> Result<bool, AppError> {
    let scripts_root = root.join("scripts");
    match fs::metadata(&scripts_root) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(AppError::FilesystemOperation {
            action: "inspect trust metadata",
            path: scripts_root,
            source,
        }),
    }
}
