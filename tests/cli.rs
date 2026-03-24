use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use serde_yaml::Value as YamlValue;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static TEST_WORKSPACE_COUNTER: AtomicU64 = AtomicU64::new(0);

const MINIMAL_LOCKFILE: &str = concat!(
    "version: 1\n",
    "\n",
    "state:\n",
    "  manifest_version: 1\n",
    "  local_state_version: 2\n",
);

#[path = "cli/bundled.rs"]
mod bundled;
#[path = "cli/cleanup.rs"]
mod cleanup;
#[path = "cli/help.rs"]
mod help;
#[path = "cli/init.rs"]
mod init;
#[path = "cli/install.rs"]
mod install;
#[path = "cli/inventory.rs"]
mod inventory;
#[path = "cli/override_cmd.rs"]
mod override_cmd;
#[path = "cli/projection.rs"]
mod projection;
#[path = "cli/update.rs"]
mod update;

#[derive(Clone, Debug, Eq, PartialEq)]
enum PathSnapshot {
    Missing,
    File(Vec<u8>),
    Directory(BTreeMap<String, PathSnapshot>),
    Symlink(PathBuf),
}

fn initialize_runtime_state(workspace: &TestWorkspace) {
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

fn assert_transaction_rolled_back(
    workspace: &TestWorkspace,
    failpoint: &str,
    source_date_epoch: &str,
    args: &[&str],
    expected: &BTreeMap<String, PathSnapshot>,
) {
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", source_date_epoch)
        .env("SKILLCTL_FAILPOINT", failpoint)
        .args(args)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("injected lifecycle failure"));

    let actual = workspace.snapshot_paths(&expected.keys().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(&actual, expected);
}

fn snapshot_path(path: &Path) -> PathSnapshot {
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

struct TestWorkspace {
    path: PathBuf,
}

impl TestWorkspace {
    fn new() -> Self {
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

    fn path(&self) -> &Path {
        &self.path
    }

    fn home_path(&self) -> PathBuf {
        let path = self.path.join("home");
        fs::create_dir_all(&path).expect("home directory exists");
        path
    }

    fn workspace_import_root_relative(&self, import_id: &str) -> PathBuf {
        let canonical = fs::canonicalize(&self.path).expect("workspace path canonicalizes");
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        PathBuf::from("home/.skillctl/store/imports/workspace")
            .join(format!("{:x}", hasher.finalize()))
            .join(import_id)
    }

    fn workspace_import_root(&self, import_id: &str) -> PathBuf {
        self.path
            .join(self.workspace_import_root_relative(import_id))
    }

    fn write_manifest(&self, contents: &str) {
        self.write_file(".agents/skillctl.yaml", contents);
    }

    fn write_lockfile(&self, contents: &str) {
        self.write_file(".agents/skillctl.lock", contents);
    }

    fn write_workspace_skill(
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

    fn write_file(&self, relative_path: &str, contents: &str) {
        let path = self.path.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directory exists");
        }
        fs::write(path, contents).expect("file written");
    }

    fn write_skill_source(&self, source_dir: &str, skill_name: &str) {
        self.write_skill_source_at(
            source_dir,
            &format!(".agents/skills/{skill_name}"),
            skill_name,
            "Summarize release notes.",
        );
    }

    fn write_skill_source_at(
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

    fn read_lockfile_yaml(&self) -> YamlValue {
        serde_yaml::from_str(
            &fs::read_to_string(self.path().join(".agents/skillctl.lock"))
                .expect("lockfile exists"),
        )
        .expect("lockfile is valid yaml")
    }

    fn snapshot_paths(&self, relative_paths: &[&str]) -> BTreeMap<String, PathSnapshot> {
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

    fn git_repo_url(&self, relative_path: &str) -> String {
        let path = fs::canonicalize(self.path.join(relative_path)).expect("repo path exists");
        format!("file://{}", path.display())
    }

    fn init_git_repo(&self, relative_path: &str) {
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

    fn commit_all(&self, relative_path: &str, message: &str) {
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

    fn git_head(&self, relative_path: &str) -> String {
        self.git_stdout(relative_path, &["rev-parse", "HEAD"])
    }

    fn run_git(&self, repo_path: &Path, args: &[&str]) {
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

    fn git_stdout(&self, relative_path: &str, args: &[&str]) -> String {
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
