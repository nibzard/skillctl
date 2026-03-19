mod support;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::{fs, path::Path};

use support::{MINIMAL_LOCKFILE, TestWorkspace};

const ALL_WORKSPACE_PROJECTION_ROOTS: &[&str] = &[
    ".claude/skills",
    ".github/skills",
    ".gemini/skills",
    ".opencode/skills",
];
const ALL_USER_PROJECTION_ROOTS: &[&str] = &[
    "~/.agents/skills",
    "~/.claude/skills",
    "~/.copilot/skills",
    "~/.config/agents/skills",
    "~/.config/amp/skills",
    "~/.config/opencode/skills",
    "~/.gemini/skills",
];

#[derive(Clone, Copy)]
struct MatrixCase {
    name: &'static str,
    targets: &'static [&'static str],
    expected_roots: &'static [&'static str],
}

#[test]
fn workspace_copy_mode_matrix_matches_spec_target_combinations() {
    let cases = [
        MatrixCase {
            name: "codex",
            targets: &["codex"],
            expected_roots: &[".agents/skills"],
        },
        MatrixCase {
            name: "gemini-cli",
            targets: &["gemini-cli"],
            expected_roots: &[".agents/skills"],
        },
        MatrixCase {
            name: "claude-code",
            targets: &["claude-code"],
            expected_roots: &[".claude/skills"],
        },
        MatrixCase {
            name: "github-copilot",
            targets: &["github-copilot"],
            expected_roots: &[".github/skills"],
        },
        MatrixCase {
            name: "amp",
            targets: &["amp"],
            expected_roots: &[".agents/skills"],
        },
        MatrixCase {
            name: "opencode",
            targets: &["opencode"],
            expected_roots: &[".agents/skills"],
        },
        MatrixCase {
            name: "codex+gemini-cli+opencode",
            targets: &["codex", "gemini-cli", "opencode"],
            expected_roots: &[".agents/skills"],
        },
        MatrixCase {
            name: "claude-code+github-copilot",
            targets: &["claude-code", "github-copilot"],
            expected_roots: &[".claude/skills"],
        },
    ];

    for case in cases {
        let workspace = TestWorkspace::new();
        workspace.copy_fixture("canonical-only", "");
        workspace.write_manifest_for_targets(case.targets);
        workspace.write_lockfile(MINIMAL_LOCKFILE);

        let assert = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
            .args(["--json", "sync"])
            .assert()
            .success()
            .stderr(predicate::str::is_empty());
        let body = json_body(&assert.get_output().stdout);

        assert_eq!(
            collect_paths(&body["data"]["plan"]["physical_roots"]),
            case.expected_roots
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>(),
            "unexpected workspace roots for {}",
            case.name
        );

        let expected_canonical_roots = case
            .expected_roots
            .iter()
            .copied()
            .filter(|root| *root == ".agents/skills")
            .collect::<Vec<_>>();
        let expected_generated_roots = case
            .expected_roots
            .iter()
            .copied()
            .filter(|root| *root != ".agents/skills")
            .collect::<Vec<_>>();

        assert_eq!(
            collect_strings(&body["data"]["canonical_roots"]),
            expected_canonical_roots
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>(),
            "unexpected canonical roots for {}",
            case.name
        );
        assert_eq!(
            collect_paths(&body["data"]["generated_roots"]),
            expected_generated_roots
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>(),
            "unexpected generated roots for {}",
            case.name
        );
        assert_eq!(body["data"]["mode"], "copy");

        assert!(
            !workspace
                .path()
                .join(".agents/skills/release-notes/.skillctl-projection.json")
                .exists(),
            "canonical local skills must not receive generated metadata in {}",
            case.name
        );

        for root in expected_generated_roots {
            assert!(
                workspace
                    .path()
                    .join(root)
                    .join("release-notes/SKILL.md")
                    .is_file(),
                "expected generated projection at '{}' for {}",
                root,
                case.name
            );
        }

        for root in ALL_WORKSPACE_PROJECTION_ROOTS {
            if case.expected_roots.contains(root) {
                continue;
            }
            assert!(
                !workspace.path().join(root).join("release-notes").exists(),
                "unexpected generated projection at '{}' for {}",
                root,
                case.name
            );
        }
    }
}

#[test]
fn user_copy_mode_matrix_matches_spec_target_combinations() {
    let cases = [
        MatrixCase {
            name: "codex",
            targets: &["codex"],
            expected_roots: &["~/.agents/skills"],
        },
        MatrixCase {
            name: "gemini-cli",
            targets: &["gemini-cli"],
            expected_roots: &["~/.agents/skills"],
        },
        MatrixCase {
            name: "claude-code",
            targets: &["claude-code"],
            expected_roots: &["~/.claude/skills"],
        },
        MatrixCase {
            name: "github-copilot",
            targets: &["github-copilot"],
            expected_roots: &["~/.copilot/skills"],
        },
        MatrixCase {
            name: "amp",
            targets: &["amp"],
            expected_roots: &["~/.config/agents/skills"],
        },
        MatrixCase {
            name: "opencode",
            targets: &["opencode"],
            expected_roots: &["~/.agents/skills"],
        },
        MatrixCase {
            name: "codex+gemini-cli+opencode",
            targets: &["codex", "gemini-cli", "opencode"],
            expected_roots: &["~/.agents/skills"],
        },
        MatrixCase {
            name: "claude-code+github-copilot",
            targets: &["claude-code", "github-copilot"],
            expected_roots: &["~/.claude/skills"],
        },
    ];

    for case in cases {
        let workspace = TestWorkspace::new();
        workspace.copy_fixture("canonical-only", "source");
        workspace.write_manifest_for_targets(case.targets);
        workspace.write_lockfile(MINIMAL_LOCKFILE);

        let home_path = workspace.home_path();
        let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");
        cmd.current_dir(workspace.path())
            .env("HOME", &home_path)
            .arg("--json")
            .arg("--no-input")
            .arg("--name")
            .arg("release-notes")
            .arg("--scope")
            .arg("user");
        for target in case.targets {
            cmd.arg("--target").arg(target);
        }
        let assert = cmd
            .arg("install")
            .arg("source")
            .assert()
            .success()
            .stderr(predicate::str::is_empty());
        let body = json_body(&assert.get_output().stdout);

        assert_eq!(body["data"]["installed"][0]["scope"], "user");
        assert_eq!(
            collect_paths(&body["data"]["projection"]["plan"]["physical_roots"]),
            case.expected_roots
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>(),
            "unexpected user roots for {}",
            case.name
        );
        assert_eq!(
            collect_strings(&body["data"]["projection"]["canonical_roots"]),
            Vec::<String>::new(),
            "user scope should not reuse workspace canonical roots for {}",
            case.name
        );
        assert_eq!(body["data"]["projection"]["mode"], "copy");

        for root in case.expected_roots {
            assert!(
                home_path
                    .join(home_relative_root(root))
                    .join("release-notes/SKILL.md")
                    .is_file(),
                "expected user projection at '{}' for {}",
                root,
                case.name
            );
        }

        for root in ALL_USER_PROJECTION_ROOTS {
            if case.expected_roots.contains(root) {
                continue;
            }
            assert!(
                !home_path
                    .join(home_relative_root(root))
                    .join("release-notes")
                    .exists(),
                "unexpected user projection at '{}' for {}",
                root,
                case.name
            );
        }
    }
}

#[test]
fn alternate_source_layout_fixtures_install_successfully() {
    #[derive(Clone, Copy)]
    struct LayoutCase {
        fixture: &'static str,
        source_path: &'static str,
        skill: &'static str,
        target: &'static str,
        expected_subpath: &'static str,
    }

    let cases = [
        LayoutCase {
            fixture: "opencode-layout",
            source_path: "source",
            skill: "ai-sdk",
            target: "opencode",
            expected_subpath: ".opencode/skills/ai-sdk",
        },
        LayoutCase {
            fixture: "repo-root-layout",
            source_path: "source",
            skill: "ai-sdk",
            target: "codex",
            expected_subpath: "skills/ai-sdk",
        },
        LayoutCase {
            fixture: "nested-monorepo",
            source_path: "source/packages/platform",
            skill: "bug-triage",
            target: "claude-code",
            expected_subpath: ".agents/skills/bug-triage",
        },
    ];

    for case in cases {
        let workspace = TestWorkspace::new();
        workspace.copy_fixture(case.fixture, "source");
        workspace.write_manifest_for_targets(&[case.target]);
        workspace.write_lockfile(MINIMAL_LOCKFILE);

        let assert = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
            .args([
                "--json",
                "--no-input",
                "--name",
                case.skill,
                "--target",
                case.target,
                "install",
                case.source_path,
            ])
            .assert()
            .success()
            .stderr(predicate::str::is_empty());
        let body = json_body(&assert.get_output().stdout);

        assert_eq!(body["data"]["selected"][0]["name"], case.skill);
        assert_eq!(
            body["data"]["selected"][0]["source_path"],
            case.expected_subpath
        );
    }
}

#[test]
fn same_name_conflict_fixture_requires_explicit_disambiguation() {
    let workspace = TestWorkspace::new();
    workspace.copy_fixture("same-name-conflict", "source");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--no-input", "--name", "release-notes", "install", "source"])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains(
            "exact skill name 'release-notes' is ambiguous",
        ));
}

#[test]
fn imported_overlay_fixture_supports_the_override_workflow() {
    let workspace = TestWorkspace::new();
    workspace.copy_fixture("imported-with-overlay", "source");
    workspace.write_manifest_for_targets(&["claude-code"]);
    workspace.write_lockfile(MINIMAL_LOCKFILE);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--no-input", "--name", "release-notes", "install", "source"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .arg("override")
        .arg("release-notes")
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    assert!(
        workspace
            .path()
            .join(".agents/overlays/release-notes/SKILL.md")
            .is_file(),
        "override should create the overlay manifest"
    );
    assert!(
        fs::read_to_string(workspace.path().join(".agents/skillctl.yaml"))
            .expect("manifest exists")
            .contains("release-notes: .agents/overlays/release-notes"),
        "override should wire the fixture import into the manifest"
    );
}

#[test]
fn update_blocker_fixture_reports_a_blocked_plan_after_runtime_edits() {
    let workspace = TestWorkspace::new();
    workspace.copy_fixture("update-blocker", "git-source");
    workspace.write_manifest_for_targets(&["claude-code"]);
    workspace.init_git_repo("git-source");
    let repo_url = workspace.git_repo_url("git-source");

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            repo_url.as_str(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    workspace.write_file(
        ".claude/skills/release-notes/SKILL.md",
        concat!(
            "---\n",
            "name: release-notes\n",
            "description: Directly edited runtime copy.\n",
            "---\n",
            "\n",
            "# release-notes\n"
        ),
    );
    workspace.write_file(
        "git-source/.agents/skills/release-notes/SKILL.md",
        concat!(
            "---\n",
            "name: release-notes\n",
            "description: Updated upstream release notes helper.\n",
            "---\n",
            "\n",
            "# release-notes\n"
        ),
    );
    workspace.commit_all("git-source", "update release notes");

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "update", "release-notes"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body = json_body(&assert.get_output().stdout);
    let plan = &body["data"]["plans"][0];

    assert_eq!(plan["outcome"], "blocked");
    assert_eq!(plan["recommended_action"], "create-overlay");
    assert_eq!(plan["modifications"][0]["kind"], "projected-copy");
}

#[test]
fn private_source_fixture_installs_as_a_local_path_source() {
    let workspace = TestWorkspace::new();
    workspace.copy_fixture("private-source", "private-source");
    workspace.write_manifest_for_targets(&["codex"]);
    workspace.write_lockfile(MINIMAL_LOCKFILE);

    let assert = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--json",
            "--no-input",
            "--name",
            "private-skill",
            "install",
            "private-source",
        ])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
    let body = json_body(&assert.get_output().stdout);

    assert_eq!(body["data"]["source"]["type"], "local-path");
    assert_eq!(body["data"]["installed"][0]["name"], "private-skill");
}

#[cfg(unix)]
#[test]
fn broken_symlink_fixture_refuses_managed_import_installation() {
    let workspace = TestWorkspace::new();
    workspace.copy_fixture("broken-symlink", "source");
    workspace.write_manifest_for_targets(&["codex"]);
    workspace.write_lockfile(MINIMAL_LOCKFILE);

    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--no-input", "--name", "release-notes", "install", "source"])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains(
            "symlinked install source entries are not supported for stored imports",
        ));
}

fn json_body(output: &[u8]) -> Value {
    serde_json::from_slice(output).expect("stdout is valid json")
}

fn collect_paths(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("array exists")
        .iter()
        .map(|entry| entry["path"].as_str().expect("path exists").to_string())
        .collect()
}

fn collect_strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("array exists")
        .iter()
        .map(|entry| entry.as_str().expect("string exists").to_string())
        .collect()
}

fn home_relative_root(root: &str) -> &Path {
    Path::new(root.strip_prefix("~/").expect("root uses ~/"))
}
