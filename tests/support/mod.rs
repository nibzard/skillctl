use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static TEST_WORKSPACE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub const MINIMAL_LOCKFILE: &str = concat!(
    "version: 1\n",
    "\n",
    "state:\n",
    "  manifest_version: 1\n",
    "  local_state_version: 2\n",
);

pub struct TestWorkspace {
    path: PathBuf,
}

impl TestWorkspace {
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

    pub fn write_file(&self, relative_path: &str, contents: &str) {
        let path = self.path.join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directory exists");
        }
        fs::write(path, contents).expect("file written");
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
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
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
