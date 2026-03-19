//! Shared schema-version policy for manifest, lockfile, and local state.
//!
//! Each state-bearing document evolves independently. The policy objects in
//! this module make version acceptance explicit and leave room for deliberate
//! migrations instead of silent breakage.

/// Current schema version for `.agents/skillctl.yaml`.
pub const CURRENT_MANIFEST_VERSION: u32 = 1;
/// Current schema version for `.agents/skillctl.lock`.
pub const CURRENT_LOCKFILE_VERSION: u32 = 1;
/// Current schema version for `~/.skillctl/state.db`.
pub const CURRENT_LOCAL_STATE_VERSION: u32 = 1;

/// Version policy for workspace manifests.
pub const MANIFEST_SCHEMA_POLICY: SchemaVersionPolicy =
    SchemaVersionPolicy::new(CURRENT_MANIFEST_VERSION, CURRENT_MANIFEST_VERSION);
/// Version policy for workspace lockfiles.
pub const LOCKFILE_SCHEMA_POLICY: SchemaVersionPolicy =
    SchemaVersionPolicy::new(CURRENT_LOCKFILE_VERSION, CURRENT_LOCKFILE_VERSION);
/// Version policy for the local state store.
pub const LOCAL_STATE_SCHEMA_POLICY: SchemaVersionPolicy =
    SchemaVersionPolicy::new(CURRENT_LOCAL_STATE_VERSION, CURRENT_LOCAL_STATE_VERSION);

/// Declarative schema version policy for a state-bearing document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchemaVersionPolicy {
    current: u32,
    minimum_supported: u32,
}

impl SchemaVersionPolicy {
    /// Create a version policy with a current and minimum supported version.
    pub const fn new(current: u32, minimum_supported: u32) -> Self {
        Self {
            current,
            minimum_supported,
        }
    }

    /// Return the current schema version.
    pub const fn current(self) -> u32 {
        self.current
    }

    /// Return the oldest schema version that remains readable.
    pub const fn minimum_supported(self) -> u32 {
        self.minimum_supported
    }

    /// Classify a discovered schema version against this policy.
    pub const fn classify(self, found: u32) -> VersionDisposition {
        if found == self.current {
            VersionDisposition::Current
        } else if found >= self.minimum_supported && found < self.current {
            VersionDisposition::NeedsMigration {
                from: found,
                to: self.current,
            }
        } else {
            VersionDisposition::Unsupported {
                found,
                minimum_supported: self.minimum_supported,
                current: self.current,
            }
        }
    }
}

/// Outcome of comparing a discovered schema version to a policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VersionDisposition {
    /// The discovered version matches the current version.
    Current,
    /// The discovered version is readable but must be migrated.
    NeedsMigration {
        /// The discovered version on disk.
        from: u32,
        /// The current version the tool writes.
        to: u32,
    },
    /// The discovered version is outside the supported range.
    Unsupported {
        /// The discovered version on disk.
        found: u32,
        /// The oldest readable version.
        minimum_supported: u32,
        /// The current version the tool writes.
        current: u32,
    },
}
