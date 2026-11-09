//! Shared integration-test harness.
//!
//! Every integration test binary that needs a scratch workspace declares
//! `mod support;` and uses [`TestWorkspace`]. Keep this module the only
//! definition: a second copy drifts, and the drift already diverged the
//! lockfile constant and the symlink behavior of `copy_tree` once.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    sync::LazyLock,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::Connection;
use serde_yaml::Value as YamlValue;
use sha2::{Digest, Sha256};

static TEST_WORKSPACE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Recorded shape of one path inside a workspace.
///
/// SQLite databases are compared by table rows. A restored database is
/// written through SQLite, which always rewrites the file header change
/// counter, so identical rows do not produce identical bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PathSnapshot {
    Missing,
    File(Vec<u8>),
    Directory(BTreeMap<String, PathSnapshot>),
    Symlink(PathBuf),
    /// Rows of every table, keyed by table name.
    Database(BTreeMap<String, Vec<String>>),
}

/// Magic prefix of every SQLite 3 database file.
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Whether the path holds a SQLite 3 database file.
pub fn is_sqlite_database(path: &Path) -> bool {
    use std::io::Read as _;

    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut magic = [0_u8; 16];
    file.read_exact(&mut magic).is_ok() && &magic == SQLITE_MAGIC
}

fn snapshot_database_rows(path: &Path) -> BTreeMap<String, Vec<String>> {
    let connection = Connection::open(path).expect("database opens");
    let mut table_statement = connection
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .expect("table query prepares");
    let tables = table_statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("table query runs")
        .collect::<Result<Vec<_>, _>>()
        .expect("table names read");

    let mut dump = BTreeMap::new();
    for table in tables {
        let mut statement = connection
            .prepare(&format!("SELECT * FROM \"{table}\""))
            .expect("row query prepares");
        let columns = statement.column_count();
        let mut rows = statement
            .query_map([], |row| {
                (0..columns)
                    .map(|index| row.get::<_, rusqlite::types::Value>(index))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|values| format!("{values:?}"))
            })
            .expect("row query runs")
            .collect::<Result<Vec<_>, _>>()
            .expect("rows read");
        rows.sort();
        dump.insert(table, rows);
    }
    dump
}

/// Record the shape of one path: missing, file bytes, symlink target,
/// directory tree, or database rows.
pub fn snapshot_path(path: &Path) -> PathSnapshot {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return PathSnapshot::Missing;
        }
        Err(error) => panic!("failed to inspect '{}': {error}", path.display()),
    };

    if metadata.file_type().is_symlink() {
        return PathSnapshot::Symlink(fs::read_link(path).unwrap_or_else(|error| {
            panic!("failed to read symlink '{}': {error}", path.display())
        }));
    }

    if metadata.is_file() {
        if is_sqlite_database(path) {
            return PathSnapshot::Database(snapshot_database_rows(path));
        }
        return PathSnapshot::File(
            fs::read(path)
                .unwrap_or_else(|error| panic!("failed to read '{}': {error}", path.display())),
        );
    }

    if metadata.is_dir() {
        let mut children = BTreeMap::new();
        let mut entries = fs::read_dir(path)
            .unwrap_or_else(|error| {
                panic!("failed to read directory '{}': {error}", path.display())
            })
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|error| {
                panic!(
                    "failed to read directory entry '{}': {error}",
                    path.display()
                )
            });
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            children.insert(name, snapshot_path(&entry.path()));
        }
        return PathSnapshot::Directory(children);
    }

    panic!("unsupported filesystem entry '{}'", path.display());
}

/// Lockfile text `skillctl init` writes, pinned to the state and lockfile
/// schema versions this build ships.
pub static MINIMAL_LOCKFILE: LazyLock<String> = LazyLock::new(|| {
    format!(
        "version: {}\n\nstate:\n  manifest_version: {}\n  local_state_version: {}\n",
        skillctl::state::CURRENT_LOCKFILE_VERSION,
        skillctl::state::CURRENT_MANIFEST_VERSION,
        skillctl::state::CURRENT_LOCAL_STATE_VERSION,
    )
});

/// One throwaway workspace directory a test commands run inside.
pub struct TestWorkspace {
    path: PathBuf,
}

impl TestWorkspace {
    // A `Default` impl would promise a meaningfully empty workspace, but the
    // workspace path only exists once `new` creates its scratch directory.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time moved backwards")
            .as_nanos();
        let counter = TEST_WORKSPACE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "skillctl-test-{}-{unique}-{counter}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("workspace exists");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn home_path(&self) -> PathBuf {
        let path = self.path.join("home");
        fs::create_dir_all(&path).expect("home directory exists");
        path
    }

    /// Relative root of one workspace-scope import inside the shared home.
    ///
    /// The import store keys a workspace by the digest of its canonical path,
    /// so the same derivation the binary uses locates the stored bytes.
    pub fn workspace_import_root_relative(&self, import_id: &str) -> PathBuf {
        let canonical = fs::canonicalize(&self.path).expect("workspace path canonicalizes");
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        PathBuf::from("home/.skillctl/store/imports/workspace")
            .join(format!("{:x}", hasher.finalize()))
            .join(import_id)
    }

    pub fn workspace_import_root(&self, import_id: &str) -> PathBuf {
        self.path
            .join(self.workspace_import_root_relative(import_id))
    }

    pub fn write_manifest(&self, contents: &str) {
        self.write_file(".agents/skillctl.yaml", contents);
    }

    /// Manifest that selects projection targets and nothing else.
    pub fn write_manifest_for_targets(&self, targets: &[&str]) {
        let mut contents = String::from("version: 1\n\ntargets:\n");
        for target in targets {
            contents.push_str("  - ");
            contents.push_str(target);
            contents.push('\n');
        }
        self.write_file(".agents/skillctl.yaml", &contents);
    }

    pub fn write_lockfile(&self, contents: &str) {
        self.write_file(".agents/skillctl.lock", contents);
    }

    pub fn write_workspace_skill(
        &self,
        skill_name: &str,
        description: &str,
        extra_files: &[(&str, &str)],
    ) {
        let skill_root = self.path.join(".agents/skills").join(skill_name);
        fs::create_dir_all(&skill_root).expect("skill source root exists");
        fs::write(
            skill_root.join("SKILL.md"),
            format!(
                concat!(
                    "---\n",
                    "name: {skill_name}\n",
                    "description: {description}\n",
                    "---\n",
                    "\n",
                    "# {skill_name}\n"
                ),
                skill_name = skill_name,
                description = description,
            ),
        )
        .expect("skill manifest exists");

        for (relative_path, contents) in extra_files {
            let path = skill_root.join(relative_path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("extra file parent exists");
            }
            fs::write(path, contents).expect("extra file written");
        }
    }

    pub fn write_file(&self, relative_path: &str, contents: &str) {
        let path = self.path.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directory exists");
        }
        fs::write(path, contents).expect("file written");
    }

    /// Copy a file or directory tree into the workspace at a relative path.
    ///
    /// Symlinks in the source are preserved as symlinks, matching
    /// [`copy_tree`].
    pub fn copy_path(&self, source: &Path, relative_path: &str) {
        let destination = self.path.join(relative_path);
        copy_tree(source, &destination).unwrap_or_else(|error| {
            panic!(
                "failed to copy '{}' into '{}': {error}",
                source.display(),
                destination.display()
            )
        });
    }

    pub fn copy_fixture(&self, fixture_name: &str, destination_relative_path: &str) {
        let source = fixture_root().join(fixture_name);
        assert!(source.is_dir(), "fixture '{}' is missing", source.display());

        let destination = if destination_relative_path.is_empty() {
            self.path.clone()
        } else {
            self.path.join(destination_relative_path)
        };

        copy_tree(&source, &destination).unwrap_or_else(|error| {
            panic!(
                "failed to copy fixture '{}' into '{}': {error}",
                source.display(),
                destination.display()
            )
        });
    }

    pub fn write_skill_source(&self, source_dir: &str, skill_name: &str) {
        self.write_skill_source_at(
            source_dir,
            &format!(".agents/skills/{skill_name}"),
            skill_name,
            "Summarize release notes.",
        );
    }

    pub fn write_skill_source_at(
        &self,
        source_dir: &str,
        relative_skill_root: &str,
        skill_name: &str,
        description: &str,
    ) {
        let skill_root = self.path.join(source_dir).join(relative_skill_root);
        fs::create_dir_all(&skill_root).expect("skill source root exists");
        fs::write(
            skill_root.join("SKILL.md"),
            format!(
                concat!(
                    "---\n",
                    "name: {skill_name}\n",
                    "description: {description}\n",
                    "---\n",
                    "\n",
                    "# {skill_name}\n"
                ),
                skill_name = skill_name,
                description = description,
            ),
        )
        .expect("skill manifest exists");
    }

    pub fn read_lockfile_yaml(&self) -> YamlValue {
        serde_yaml::from_str(
            &fs::read_to_string(self.path().join(".agents/skillctl.lock"))
                .expect("lockfile exists"),
        )
        .expect("lockfile is valid yaml")
    }

    pub fn snapshot_paths(&self, relative_paths: &[&str]) -> BTreeMap<String, PathSnapshot> {
        relative_paths
            .iter()
            .map(|relative_path| {
                (
                    (*relative_path).to_string(),
                    snapshot_path(&self.path.join(relative_path)),
                )
            })
            .collect()
    }

    pub fn git_repo_url(&self, relative_path: &str) -> String {
        let path = fs::canonicalize(self.path.join(relative_path)).expect("repo path exists");
        format!("file://{}", path.display())
    }

    pub fn init_git_repo(&self, relative_path: &str) {
        let repo_path = self.path.join(relative_path);
        self.run_git(&repo_path, &["init", "--initial-branch", "main"]);
        self.run_git(&repo_path, &["config", "user.name", "Skillctl Tests"]);
        self.run_git(
            &repo_path,
            &["config", "user.email", "skillctl-tests@example.com"],
        );
        self.run_git(&repo_path, &["add", "."]);
        self.run_git(
            &repo_path,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "initial import",
            ],
        );
    }

    pub fn commit_all(&self, relative_path: &str, message: &str) {
        let repo_path = self.path.join(relative_path);
        self.run_git(&repo_path, &["add", "."]);
        self.run_git(
            &repo_path,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                message,
            ],
        );
    }

    /// Current `HEAD` commit of one repository below the workspace.
    pub fn git_head(&self, relative_path: &str) -> String {
        self.git_stdout(relative_path, &["rev-parse", "HEAD"])
    }

    pub fn run_git(&self, repo_path: &Path, args: &[&str]) {
        let status = ProcessCommand::new("git")
            .current_dir(repo_path)
            .env("GIT_AUTHOR_NAME", "Skillctl Tests")
            .env("GIT_AUTHOR_EMAIL", "skillctl-tests@example.com")
            .env("GIT_COMMITTER_NAME", "Skillctl Tests")
            .env("GIT_COMMITTER_EMAIL", "skillctl-tests@example.com")
            .args(args)
            .status()
            .expect("git command launches");
        assert!(
            status.success(),
            "git {:?} failed in {}",
            args,
            repo_path.display()
        );
    }

    pub fn git_stdout(&self, relative_path: &str, args: &[&str]) -> String {
        let repo_path = self.path.join(relative_path);
        let output = ProcessCommand::new("git")
            .current_dir(&repo_path)
            .env("GIT_AUTHOR_NAME", "Skillctl Tests")
            .env("GIT_AUTHOR_EMAIL", "skillctl-tests@example.com")
            .env("GIT_COMMITTER_NAME", "Skillctl Tests")
            .env("GIT_COMMITTER_EMAIL", "skillctl-tests@example.com")
            .args(args)
            .output()
            .expect("git command launches");
        assert!(
            output.status.success(),
            "git {:?} failed in {}",
            args,
            repo_path.display()
        );
        String::from_utf8(output.stdout)
            .expect("git stdout is valid utf-8")
            .trim()
            .to_string()
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Recursively copy a file or directory tree, preserving symlinks as symlinks.
///
/// The broken-symlink fixture relies on a dangling link staying a link instead
/// of its (missing) target being read, and [`TestWorkspace::copy_path`] shares
/// the rule so both harness callers behave the same.
pub fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        copy_symlink(source, destination)
    } else if metadata.is_dir() {
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
        Ok(())
    }
}

fn copy_symlink(source: &Path, destination: &Path) -> io::Result<()> {
    let target = fs::read_link(source)?;
    create_file_symlink(&target, destination)
}

#[cfg(unix)]
fn create_file_symlink(target: &Path, destination: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, destination)
}

#[cfg(windows)]
fn create_file_symlink(target: &Path, destination: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, destination)
}
