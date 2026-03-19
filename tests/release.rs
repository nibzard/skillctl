#![cfg(unix)]

#[allow(dead_code)]
mod support;

use assert_cmd::cargo::cargo_bin;
use serde::Deserialize;
use serde_yaml::{Mapping, Value as YamlValue};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

use support::TestWorkspace;

const RELEASE_VERSION: &str = "v0.1.0-test";
const LINUX_RELEASE_TARGET: &str = "x86_64-unknown-linux-gnu";
const WINDOWS_RELEASE_TARGET: &str = "x86_64-pc-windows-msvc";
const ZIP_DEFLATED: u16 = 8;

#[test]
fn packaging_script_produces_deterministic_archives_for_same_inputs() {
    let workspace = TestWorkspace::new();
    let binary_path = cargo_bin("skillctl");

    let first_archive = package_release(
        &binary_path,
        &workspace.path().join("dist-one"),
        LINUX_RELEASE_TARGET,
    );
    let second_archive = package_release(
        &binary_path,
        &workspace.path().join("dist-two"),
        LINUX_RELEASE_TARGET,
    );

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
    let archive_path = package_release(
        &binary_path,
        &workspace.path().join("dist"),
        LINUX_RELEASE_TARGET,
    );
    let unpack_root = workspace.path().join("unpacked");
    let run_root = workspace.path().join("isolated-run");
    fs::create_dir_all(&unpack_root).expect("unpack root exists");
    fs::create_dir_all(&run_root).expect("isolated run root exists");

    extract_archive(&archive_path, &unpack_root);

    let packaged_binary = unpack_root
        .join(release_archive_stem(LINUX_RELEASE_TARGET))
        .join(binary_name(LINUX_RELEASE_TARGET));
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
    let archive_path = package_release(
        &binary_path,
        &workspace.path().join("dist"),
        LINUX_RELEASE_TARGET,
    );
    let release_root = workspace.path().join("release-root");
    let download_root = release_root.join("download").join(RELEASE_VERSION);
    let install_dir = workspace.path().join("bin");
    fs::create_dir_all(&download_root).expect("release download root exists");
    fs::create_dir_all(&install_dir).expect("install dir exists");

    let archive_name = archive_name(LINUX_RELEASE_TARGET);
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
fn packaging_script_produces_deterministic_windows_zip_archives_for_same_inputs() {
    let workspace = TestWorkspace::new();
    let binary_path = cargo_bin("skillctl");

    let first_archive = package_release(
        &binary_path,
        &workspace.path().join("windows-dist-one"),
        WINDOWS_RELEASE_TARGET,
    );
    let second_archive = package_release(
        &binary_path,
        &workspace.path().join("windows-dist-two"),
        WINDOWS_RELEASE_TARGET,
    );

    assert_eq!(
        sha256_file(&first_archive),
        sha256_file(&second_archive),
        "windows release zips should be byte-for-byte reproducible for the same inputs"
    );
}

#[test]
fn windows_release_zip_uses_expected_layout_and_deterministic_metadata() {
    let workspace = TestWorkspace::new();
    let binary_path = cargo_bin("skillctl");
    let archive_path = package_release(
        &binary_path,
        &workspace.path().join("windows-dist"),
        WINDOWS_RELEASE_TARGET,
    );
    let entries = inspect_zip_entries(&archive_path);
    let archive_root = release_archive_stem(WINDOWS_RELEASE_TARGET);

    assert_eq!(
        archive_path.file_name().and_then(|name| name.to_str()),
        Some(archive_name(WINDOWS_RELEASE_TARGET).as_str()),
        "windows target should produce a zip artifact with the expected release filename"
    );
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.filename.as_str())
            .collect::<Vec<_>>(),
        vec![
            format!("{archive_root}/LICENSE"),
            format!("{archive_root}/README.md"),
            format!("{archive_root}/release-manifest.json"),
            format!("{archive_root}/skillctl.exe"),
        ],
        "windows release zip should contain the expected top-level layout"
    );

    for entry in &entries {
        assert_eq!(
            entry.date_time,
            vec![1980, 1, 1, 0, 0, 0],
            "{} should use the fixed DOS epoch timestamp for deterministic zips",
            entry.filename
        );
        assert_eq!(
            entry.create_system, 3,
            "{} should record Unix metadata so file modes are preserved",
            entry.filename
        );
        assert_eq!(
            entry.compress_type, ZIP_DEFLATED,
            "{} should use deflate compression in the published Windows zip",
            entry.filename
        );
    }

    assert_eq!(
        mode_for_entry(&entries, &format!("{archive_root}/skillctl.exe")),
        0o755,
        "windows binary should remain executable when unpacked on Unix hosts"
    );
    for filename in [
        format!("{archive_root}/LICENSE"),
        format!("{archive_root}/README.md"),
        format!("{archive_root}/release-manifest.json"),
    ] {
        assert_eq!(
            mode_for_entry(&entries, &filename),
            0o644,
            "{filename} should be packaged with read-only data file permissions"
        );
    }

    let manifest_entry = entries
        .iter()
        .find(|entry| entry.filename == format!("{archive_root}/release-manifest.json"))
        .expect("release manifest entry is present");
    let manifest: ReleaseManifest = serde_json::from_str(
        manifest_entry
            .contents
            .as_deref()
            .expect("release manifest contents are captured"),
    )
    .expect("release manifest should parse as json");

    assert_eq!(manifest.name, "skillctl");
    assert_eq!(manifest.version, RELEASE_VERSION);
    assert_eq!(manifest.target, WINDOWS_RELEASE_TARGET);
    assert_eq!(manifest.binary, "skillctl.exe");
    assert_eq!(
        manifest.files,
        vec![
            "skillctl.exe".to_owned(),
            "LICENSE".to_owned(),
            "README.md".to_owned(),
            "release-manifest.json".to_owned(),
        ],
        "release manifest should describe the published Windows archive contents"
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
    let workflow: YamlValue =
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

fn package_release(binary_path: &Path, output_dir: &Path, target: &str) -> PathBuf {
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
            target,
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

    output_dir.join(archive_name(target))
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

fn archive_name(target: &str) -> String {
    let extension = if is_windows_target(target) {
        "zip"
    } else {
        "tar.gz"
    };
    format!("skillctl-{RELEASE_VERSION}-{target}.{extension}")
}

fn checksums_name() -> String {
    format!("skillctl-{RELEASE_VERSION}-checksums.txt")
}

fn release_archive_stem(target: &str) -> String {
    format!("skillctl-{RELEASE_VERSION}-{target}")
}

fn file_url(path: &Path) -> String {
    format!(
        "file://{}",
        fs::canonicalize(path)
            .expect("release root is canonical")
            .display()
    )
}

fn root_mapping(value: &YamlValue) -> &Mapping {
    value.as_mapping().expect("yaml root should be a mapping")
}

fn as_mapping<'a>(value: &'a YamlValue, context: &str) -> &'a Mapping {
    value
        .as_mapping()
        .unwrap_or_else(|| panic!("{context} should be a yaml mapping"))
}

fn mapping_entry<'a>(mapping: &'a Mapping, key: &str) -> &'a YamlValue {
    let string_key = YamlValue::String(key.to_owned());
    mapping
        .get(&string_key)
        .or_else(|| {
            if key == "on" {
                mapping.get(YamlValue::Bool(true))
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
            let uses = step.get(YamlValue::String("uses".to_owned()))?.as_str()?;
            (uses == "actions/checkout@v4").then_some(step)
        })
        .unwrap_or_else(|| panic!("{job_name} should include actions/checkout@v4"))
}

fn is_windows_target(target: &str) -> bool {
    target.contains("windows")
}

fn binary_name(target: &str) -> &'static str {
    if is_windows_target(target) {
        "skillctl.exe"
    } else {
        "skillctl"
    }
}

fn inspect_zip_entries(archive_path: &Path) -> Vec<ZipEntry> {
    let python = detect_python();
    let output = ProcessCommand::new(&python)
        .arg("-c")
        .arg(
            r#"import json
import sys
import zipfile

with zipfile.ZipFile(sys.argv[1]) as archive:
    entries = []
    for info in archive.infolist():
        entry = {
            "filename": info.filename,
            "date_time": list(info.date_time),
            "create_system": info.create_system,
            "mode": (info.external_attr >> 16) & 0xFFFF,
            "compress_type": info.compress_type,
        }
        if info.filename.endswith("release-manifest.json"):
            entry["contents"] = archive.read(info).decode("utf-8")
        entries.append(entry)
    print(json.dumps(entries))
"#,
        )
        .arg(archive_path)
        .output()
        .expect("zip inspection launches");

    assert!(
        output.status.success(),
        "zip inspection should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    serde_json::from_slice(&output.stdout).expect("zip inspection output should parse")
}

fn detect_python() -> String {
    if let Some(path) = std::env::var("PYTHON")
        .ok()
        .filter(|value| !value.is_empty())
    {
        return path;
    }

    for candidate in ["python3", "python"] {
        if ProcessCommand::new(candidate)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return candidate.to_owned();
        }
    }

    panic!("python3 or python is required for release packaging tests");
}

fn mode_for_entry(entries: &[ZipEntry], filename: &str) -> u32 {
    entries
        .iter()
        .find(|entry| entry.filename == filename)
        .unwrap_or_else(|| panic!("missing zip entry {filename}"))
        .mode
}

#[derive(Debug, Deserialize)]
struct ZipEntry {
    filename: String,
    date_time: Vec<u16>,
    create_system: u8,
    mode: u32,
    compress_type: u16,
    contents: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReleaseManifest {
    name: String,
    version: String,
    target: String,
    binary: String,
    files: Vec<String>,
}
