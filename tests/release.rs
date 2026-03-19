#![cfg(unix)]

#[allow(dead_code)]
mod support;

use assert_cmd::cargo::cargo_bin;
use serde_yaml::{Mapping, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

use support::TestWorkspace;

const RELEASE_VERSION: &str = "v0.1.0-test";
const RELEASE_TARGET: &str = "x86_64-unknown-linux-gnu";

#[test]
fn packaging_script_produces_deterministic_archives_for_same_inputs() {
    let workspace = TestWorkspace::new();
    let binary_path = cargo_bin("skillctl");

    let first_archive = package_release(&binary_path, &workspace.path().join("dist-one"));
    let second_archive = package_release(&binary_path, &workspace.path().join("dist-two"));

    assert_eq!(
        sha256_file(&first_archive),
        sha256_file(&second_archive),
        "release archives should be byte-for-byte reproducible for the same inputs"
    );
}

#[test]
fn packaged_binary_bootstraps_bundled_skill_without_source_tree_assets() {
    let workspace = TestWorkspace::new();
    let binary_path = cargo_bin("skillctl");
    let archive_path = package_release(&binary_path, &workspace.path().join("dist"));
    let unpack_root = workspace.path().join("unpacked");
    let run_root = workspace.path().join("isolated-run");
    fs::create_dir_all(&unpack_root).expect("unpack root exists");
    fs::create_dir_all(&run_root).expect("isolated run root exists");

    extract_archive(&archive_path, &unpack_root);

    let packaged_binary = unpack_root.join(release_archive_stem()).join("skillctl");
    let home_path = workspace.home_path();
    let output = ProcessCommand::new(&packaged_binary)
        .current_dir(&run_root)
        .env("HOME", &home_path)
        .args(["--json", "telemetry", "status"])
        .output()
        .expect("packaged binary launches");

    assert!(
        output.status.success(),
        "packaged binary should succeed on first run: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        home_path.join(".agents/skills/skillctl/SKILL.md").exists(),
        "packaged binary should install the bundled skill into user scope on first run"
    );
}

#[test]
fn install_script_installs_from_a_versioned_release_layout() {
    let workspace = TestWorkspace::new();
    let binary_path = cargo_bin("skillctl");
    let archive_path = package_release(&binary_path, &workspace.path().join("dist"));
    let release_root = workspace.path().join("release-root");
    let download_root = release_root.join("download").join(RELEASE_VERSION);
    let install_dir = workspace.path().join("bin");
    fs::create_dir_all(&download_root).expect("release download root exists");
    fs::create_dir_all(&install_dir).expect("install dir exists");

    let archive_name = archive_name();
    fs::copy(&archive_path, download_root.join(&archive_name)).expect("archive copied");
    fs::write(
        download_root.join(checksums_name()),
        format!("{}  {archive_name}\n", sha256_file(&archive_path)),
    )
    .expect("checksums file written");

    let output = ProcessCommand::new("bash")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .arg("scripts/install.sh")
        .env("SKILLCTL_RELEASE_BASE_URL", file_url(&release_root))
        .env("SKILLCTL_VERSION", RELEASE_VERSION)
        .env("SKILLCTL_INSTALL_DIR", &install_dir)
        .output()
        .expect("installer script launches");

    assert!(
        output.status.success(),
        "installer should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let installed_binary = install_dir.join("skillctl");
    assert!(
        installed_binary.exists(),
        "installer should place the binary in the requested install dir"
    );

    let home_path = workspace.home_path();
    let version_output = ProcessCommand::new(&installed_binary)
        .env("HOME", &home_path)
        .arg("--version")
        .output()
        .expect("installed binary launches");
    assert!(
        version_output.status.success(),
        "installed binary should run: stdout={} stderr={}",
        String::from_utf8_lossy(&version_output.stdout),
        String::from_utf8_lossy(&version_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&version_output.stdout).contains("skillctl"),
        "installed binary should respond to --version"
    );
}

#[test]
fn install_script_rejects_unsupported_linux_arm64_before_download() {
    let workspace = TestWorkspace::new();
    let release_root = workspace.path().join("release-root");
    let fake_bin = workspace.path().join("fake-bin");
    fs::create_dir_all(&release_root).expect("release root exists");
    fs::create_dir_all(&fake_bin).expect("fake bin exists");

    let uname_path = fake_bin.join("uname");
    fs::write(
        &uname_path,
        concat!(
            "#!/usr/bin/env bash\n",
            "case \"$1\" in\n",
            "  -s)\n",
            "    printf 'Linux\\n'\n",
            "    ;;\n",
            "  -m)\n",
            "    printf 'aarch64\\n'\n",
            "    ;;\n",
            "  *)\n",
            "    /usr/bin/uname \"$@\"\n",
            "    ;;\n",
            "esac\n",
        ),
    )
    .expect("fake uname written");
    let mut permissions = fs::metadata(&uname_path)
        .expect("fake uname metadata available")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&uname_path, permissions).expect("fake uname is executable");

    let path = std::env::var("PATH").unwrap_or_default();
    let output = ProcessCommand::new("bash")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .arg("scripts/install.sh")
        .env("PATH", format!("{}:{path}", fake_bin.display()))
        .env("SKILLCTL_RELEASE_BASE_URL", file_url(&release_root))
        .env("SKILLCTL_VERSION", RELEASE_VERSION)
        .output()
        .expect("installer script launches");

    assert!(
        !output.status.success(),
        "installer should reject unsupported Linux arm64: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("unsupported platform Linux arm64 for published release artifacts"),
        "installer should explain the unsupported Linux arm64 platform: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn release_workflow_manual_dispatch_requires_explicit_release_tag_checkout() {
    let workflow_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/release.yml");
    let workflow_source = fs::read_to_string(&workflow_path).expect("release workflow exists");
    let workflow: Value =
        serde_yaml::from_str(&workflow_source).expect("release workflow should parse as yaml");

    let on = mapping_entry(root_mapping(&workflow), "on");
    let workflow_dispatch = mapping_entry(as_mapping(on, "workflow_dispatch"), "workflow_dispatch");
    let inputs = mapping_entry(as_mapping(workflow_dispatch, "workflow_dispatch"), "inputs");
    let release_tag = mapping_entry(
        as_mapping(inputs, "workflow_dispatch.inputs"),
        "release_tag",
    );
    let release_tag = as_mapping(release_tag, "workflow_dispatch.inputs.release_tag");

    assert!(
        mapping_bool(release_tag, "required"),
        "workflow_dispatch.release_tag should be required so manual runs cannot start without a release tag"
    );
    assert!(
        mapping_string(release_tag, "description").contains("tag"),
        "workflow_dispatch.release_tag should explain that operators must supply an existing tag"
    );

    let jobs = mapping_entry(root_mapping(&workflow), "jobs");
    let jobs = as_mapping(jobs, "jobs");
    for job_name in ["build", "publish"] {
        let job = mapping_entry(jobs, job_name);
        let job = as_mapping(job, job_name);
        let checkout = checkout_step(job, job_name);
        let with = mapping_entry(checkout, "with");
        let with = as_mapping(with, "actions/checkout.with");
        let checkout_ref = mapping_string(with, "ref");

        assert!(
            checkout_ref.contains("workflow_dispatch"),
            "{job_name} checkout should branch on workflow_dispatch runs"
        );
        assert!(
            checkout_ref.contains("inputs.release_tag"),
            "{job_name} checkout should use the manual release_tag input"
        );
        assert!(
            checkout_ref.contains("refs/tags/"),
            "{job_name} checkout should resolve manual dispatches against an explicit tag ref"
        );
    }

    assert!(
        !workflow_source.contains("GITHUB_REF_TYPE"),
        "release workflow should not reject manual dispatches based on branch-only GITHUB_REF_TYPE"
    );
}

fn package_release(binary_path: &Path, output_dir: &Path) -> PathBuf {
    fs::create_dir_all(output_dir).expect("output dir exists");

    let output = ProcessCommand::new("bash")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .arg("scripts/package-release.sh")
        .args([
            "--binary",
            binary_path.to_str().expect("binary path is utf-8"),
            "--version",
            RELEASE_VERSION,
            "--target",
            RELEASE_TARGET,
            "--output",
            output_dir.to_str().expect("output path is utf-8"),
        ])
        .output()
        .expect("packaging script launches");

    assert!(
        output.status.success(),
        "packaging script should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    output_dir.join(archive_name())
}

fn extract_archive(archive_path: &Path, destination: &Path) {
    let status = ProcessCommand::new("tar")
        .args([
            "-xzf",
            archive_path.to_str().expect("archive path is utf-8"),
        ])
        .arg("-C")
        .arg(destination)
        .status()
        .expect("tar launches");

    assert!(status.success(), "archive extraction should succeed");
}

fn sha256_file(path: &Path) -> String {
    let bytes = fs::read(path).expect("file exists");
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

fn archive_name() -> String {
    format!("skillctl-{RELEASE_VERSION}-{RELEASE_TARGET}.tar.gz")
}

fn checksums_name() -> String {
    format!("skillctl-{RELEASE_VERSION}-checksums.txt")
}

fn release_archive_stem() -> String {
    format!("skillctl-{RELEASE_VERSION}-{RELEASE_TARGET}")
}

fn file_url(path: &Path) -> String {
    format!(
        "file://{}",
        fs::canonicalize(path)
            .expect("release root is canonical")
            .display()
    )
}

fn root_mapping(value: &Value) -> &Mapping {
    value.as_mapping().expect("yaml root should be a mapping")
}

fn as_mapping<'a>(value: &'a Value, context: &str) -> &'a Mapping {
    value
        .as_mapping()
        .unwrap_or_else(|| panic!("{context} should be a yaml mapping"))
}

fn mapping_entry<'a>(mapping: &'a Mapping, key: &str) -> &'a Value {
    let string_key = Value::String(key.to_owned());
    mapping
        .get(&string_key)
        .or_else(|| {
            if key == "on" {
                mapping.get(Value::Bool(true))
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("missing yaml key {key}"))
}

fn mapping_bool(mapping: &Mapping, key: &str) -> bool {
    mapping_entry(mapping, key)
        .as_bool()
        .unwrap_or_else(|| panic!("{key} should be a boolean"))
}

fn mapping_string<'a>(mapping: &'a Mapping, key: &str) -> &'a str {
    mapping_entry(mapping, key)
        .as_str()
        .unwrap_or_else(|| panic!("{key} should be a string"))
}

fn checkout_step<'a>(job: &'a Mapping, job_name: &str) -> &'a Mapping {
    let steps = mapping_entry(job, "steps")
        .as_sequence()
        .unwrap_or_else(|| panic!("{job_name}.steps should be a sequence"));

    steps
        .iter()
        .find_map(|step| {
            let step = step.as_mapping()?;
            let uses = step.get(Value::String("uses".to_owned()))?.as_str()?;
            (uses == "actions/checkout@v4").then_some(step)
        })
        .unwrap_or_else(|| panic!("{job_name} should include actions/checkout@v4"))
}
