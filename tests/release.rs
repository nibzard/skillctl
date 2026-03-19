#![cfg(unix)]

#[allow(dead_code)]
mod support;

use assert_cmd::cargo::cargo_bin;
use sha2::{Digest, Sha256};
use std::{
    fs,
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
