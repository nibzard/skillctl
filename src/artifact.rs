//! Immutable, content-addressed local artifact storage.
//!
//! Every imported skill is retained as an artifact under
//! `~/.skillctl/store/artifacts/<bundle-digest>/`:
//!
//! ```text
//! <digest-hex>/
//!   bundle.json    canonical manifest of the retained tree
//!   content/       the retained file tree itself
//! ```
//!
//! The directory name is the canonical bundle digest, so an artifact is
//! portable across workspaces, machines, and moved checkouts. It never
//! references a workspace key, an access token, or a local database.
//!
//! Artifacts are write-once. Storing an already-present artifact verifies it and
//! leaves it untouched; a stored artifact that fails verification is reported as
//! tampering and is never overwritten in place. `skillctl verify` reports this
//! state and `skillctl restore --frozen` refuses to trust it.

use std::{
    fs,
    path::{Path, PathBuf},
};

use tempfile::TempDir;

use crate::{
    bundle::{BundleManifest, artifact_directory_name},
    error::AppError,
};

/// Directory below the skillctl home that holds retained artifacts.
pub const ARTIFACTS_STORE_RELATIVE_PATH: &str = ".skillctl/store/artifacts";
/// File name of the canonical manifest inside one artifact.
pub const ARTIFACT_MANIFEST_FILE: &str = "bundle.json";
/// Directory name of the retained tree inside one artifact.
pub const ARTIFACT_CONTENT_DIR: &str = "content";

/// Result of storing a tree in the artifact store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactWrite {
    /// The artifact was created by this call.
    Created,
    /// An identical, verified artifact already existed.
    AlreadyPresent,
}

/// The content-addressed artifact store below the skillctl home directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    /// Open the default store under the skillctl home directory.
    pub fn open_default() -> Result<Self, AppError> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .ok_or(AppError::HomeDirectoryUnavailable)?;
        Ok(Self::open_at(home.join(ARTIFACTS_STORE_RELATIVE_PATH)))
    }

    /// Open a store rooted at an explicit path. Intended for tests.
    pub fn open_at(root: PathBuf) -> Self {
        Self { root }
    }

    /// Root directory of the store.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory of one artifact.
    pub fn artifact_dir(&self, digest: &str) -> Result<PathBuf, AppError> {
        Ok(self.root.join(artifact_directory_name(digest)?))
    }

    /// Retained content directory of one artifact.
    pub fn content_dir(&self, digest: &str) -> Result<PathBuf, AppError> {
        Ok(self.artifact_dir(digest)?.join(ARTIFACT_CONTENT_DIR))
    }

    /// Stored manifest path of one artifact.
    pub fn manifest_path(&self, digest: &str) -> Result<PathBuf, AppError> {
        Ok(self.artifact_dir(digest)?.join(ARTIFACT_MANIFEST_FILE))
    }

    /// Whether an artifact directory exists for one digest.
    pub fn exists(&self, digest: &str) -> Result<bool, AppError> {
        Ok(self.artifact_dir(digest)?.exists())
    }

    /// Load and verify the stored artifact for one digest.
    ///
    /// Returns `Ok(None)` when no artifact is retained. Returns an error when an
    /// artifact is present but its manifest is unreadable or its retained tree
    /// no longer matches the recorded bundle digest.
    pub fn verify(&self, digest: &str) -> Result<Option<BundleManifest>, AppError> {
        let manifest_path = self.manifest_path(digest)?;
        let contents = match fs::read_to_string(&manifest_path) {
            Ok(contents) => contents,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                if self.artifact_dir(digest)?.exists() {
                    return Err(self.validation(format!(
                        "artifact for {digest} is incomplete: '{}' is missing",
                        manifest_path.display()
                    )));
                }
                return Ok(None);
            }
            Err(source) => {
                return Err(AppError::FilesystemOperation {
                    action: "read artifact manifest",
                    path: manifest_path,
                    source,
                });
            }
        };

        let manifest = BundleManifest::from_json_str(&contents)
            .map_err(|source| self.validation(format!("artifact for {digest}: {source}")))?;
        if manifest.digest() != digest {
            return Err(self.validation(format!(
                "artifact stored under {digest} contains a manifest that digests to {}; the \
                 artifact store entry was moved or tampered with",
                manifest.digest()
            )));
        }

        let content_dir = self.content_dir(digest)?;
        if !content_dir.is_dir() {
            return Err(self.validation(format!(
                "artifact for {digest} is incomplete: '{}' is missing",
                content_dir.display()
            )));
        }
        let diff = manifest.diff_tree(&content_dir)?;
        if !diff.is_clean() {
            return Err(self.validation(format!(
                "artifact for {digest} no longer matches its recorded bundle ({})",
                diff.describe()
            )));
        }

        Ok(Some(manifest))
    }

    /// Retain one tree as an immutable artifact.
    ///
    /// The tree is captured with the canonical bundle rules, staged beside the
    /// store, and moved into place only after the capture succeeds. An existing
    /// artifact is verified and left untouched.
    pub fn store(&self, source_root: &Path) -> Result<(BundleManifest, ArtifactWrite), AppError> {
        let manifest = BundleManifest::capture(source_root)?;
        let digest = manifest.digest();
        let artifact_dir = self.artifact_dir(&digest)?;

        if artifact_dir.exists() {
            self.verify(&digest)?;
            return Ok((manifest, ArtifactWrite::AlreadyPresent));
        }

        fs::create_dir_all(&self.root).map_err(|source| AppError::FilesystemOperation {
            action: "create artifact store root",
            path: self.root.clone(),
            source,
        })?;

        let staging =
            TempDir::new_in(&self.root).map_err(|source| AppError::FilesystemOperation {
                action: "create artifact staging directory",
                path: self.root.clone(),
                source,
            })?;
        let staged_dir = staging.path().join(artifact_directory_name(&digest)?);
        let staged_content = staged_dir.join(ARTIFACT_CONTENT_DIR);
        fs::create_dir_all(&staged_content).map_err(|source| AppError::FilesystemOperation {
            action: "create staged artifact content directory",
            path: staged_content.clone(),
            source,
        })?;

        crate::source::copy_source_tree(source_root, &staged_content)?;
        let manifest_contents = manifest.to_json_string()?;
        fs::write(staged_dir.join(ARTIFACT_MANIFEST_FILE), manifest_contents).map_err(
            |source| AppError::FilesystemOperation {
                action: "write staged artifact manifest",
                path: staged_dir.join(ARTIFACT_MANIFEST_FILE),
                source,
            },
        )?;

        // Verify what was staged before it becomes the retained artifact.
        let staged_manifest = BundleManifest::capture(&staged_content)?;
        if staged_manifest.digest() != digest {
            return Err(self.validation(format!(
                "staged artifact digests to {} but the source tree digested to {digest}",
                staged_manifest.digest()
            )));
        }

        match fs::rename(&staged_dir, &artifact_dir) {
            Ok(()) => {}
            Err(source)
                if source.kind() == std::io::ErrorKind::AlreadyExists || artifact_dir.exists() =>
            {
                // Another process created the artifact first. Trust it only if
                // it verifies.
                self.verify(&digest)?;
                return Ok((manifest, ArtifactWrite::AlreadyPresent));
            }
            Err(source) => {
                return Err(AppError::FilesystemOperation {
                    action: "publish artifact into the store",
                    path: artifact_dir,
                    source,
                });
            }
        }

        Ok((manifest, ArtifactWrite::Created))
    }

    /// Materialize one verified artifact into a destination directory.
    pub fn hydrate_into(&self, digest: &str, destination: &Path) -> Result<(), AppError> {
        let manifest = self.verify(digest)?.ok_or_else(|| {
            self.validation(format!("no verified artifact is retained for {digest}"))
        })?;
        let content_dir = self.content_dir(digest)?;
        crate::source::copy_source_tree(&content_dir, destination)?;

        let diff = manifest.diff_tree(destination)?;
        if !diff.is_clean() {
            return Err(self.validation(format!(
                "restored content for {digest} does not match the retained bundle ({})",
                diff.describe()
            )));
        }

        Ok(())
    }

    /// List the digests currently retained in the store.
    pub fn retained_digests(&self) -> Result<Vec<String>, AppError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut digests = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(|source| AppError::FilesystemOperation {
            action: "read artifact store root",
            path: self.root.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| AppError::FilesystemOperation {
                action: "read artifact store entry",
                path: self.root.clone(),
                source,
            })?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name.starts_with("tmp") {
                continue;
            }
            digests.push(format!("{}{name}", crate::bundle::BUNDLE_DIGEST_PREFIX));
        }
        digests.sort();
        Ok(digests)
    }

    fn validation(&self, message: impl Into<String>) -> AppError {
        AppError::ArtifactValidation {
            path: self.root.clone(),
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct StoreFixture {
        home: PathBuf,
    }

    impl StoreFixture {
        fn new(name: &str) -> Self {
            let home = std::env::temp_dir().join(format!(
                "skillctl-artifact-{}-{name}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("time moved backwards")
                    .as_nanos()
            ));
            fs::create_dir_all(&home).expect("fixture home exists");
            Self { home }
        }

        fn tree(&self, name: &str) -> PathBuf {
            let root = self.home.join(name);
            fs::create_dir_all(root.join("references")).expect("tree directories exist");
            fs::write(root.join("SKILL.md"), "# skill\n").expect("manifest written");
            fs::write(root.join("references/alpha.md"), "alpha\n").expect("reference written");
            root
        }

        fn store(&self) -> ArtifactStore {
            ArtifactStore::open_at(self.home.join(".skillctl/store/artifacts"))
        }
    }

    impl Drop for StoreFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.home);
        }
    }

    #[test]
    fn storing_is_idempotent_and_write_once() {
        let fixture = StoreFixture::new("idempotent");
        let tree = fixture.tree("source");
        let store = fixture.store();

        let (manifest, written) = store.store(&tree).expect("artifact stored");
        assert_eq!(written, ArtifactWrite::Created);
        let digest = manifest.digest();
        assert!(store.exists(&digest).expect("existence checked"));

        let (_, written_again) = store.store(&tree).expect("artifact stored again");
        assert_eq!(written_again, ArtifactWrite::AlreadyPresent);
        assert!(store.verify(&digest).expect("artifact verifies").is_some());
    }

    #[test]
    fn tampering_with_a_retained_artifact_is_detected() {
        let fixture = StoreFixture::new("tampered");
        let tree = fixture.tree("source");
        let store = fixture.store();
        let (manifest, _) = store.store(&tree).expect("artifact stored");
        let digest = manifest.digest();

        let retained = store
            .content_dir(&digest)
            .expect("content directory resolves")
            .join("SKILL.md");
        fs::write(&retained, "# tampered\n").expect("artifact modified");

        let error = store.verify(&digest).expect_err("tampering is detected");
        assert!(
            error.to_string().contains("modified: SKILL.md"),
            "unexpected error: {error}"
        );
        let error = store
            .store(&tree)
            .expect_err("a tampered artifact is never overwritten");
        assert!(
            error.to_string().contains("SKILL.md"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn artifacts_are_portable_across_store_roots() {
        let fixture = StoreFixture::new("portable");
        let tree = fixture.tree("source");
        let store = fixture.store();
        let (manifest, _) = store.store(&tree).expect("artifact stored");
        let digest = manifest.digest();

        let destination = fixture.home.join("workspace-store/release-notes");
        store
            .hydrate_into(&digest, &destination)
            .expect("artifact hydrates");
        assert_eq!(
            fs::read_to_string(destination.join("SKILL.md")).expect("file restored"),
            "# skill\n"
        );

        let moved_store = ArtifactStore::open_at(fixture.home.join(".skillctl/store/artifacts"));
        assert_eq!(
            moved_store.retained_digests().expect("digests listed"),
            vec![digest]
        );
    }
}
