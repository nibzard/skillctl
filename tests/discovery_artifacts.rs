//! Library-level checks for verified remote artifact fetch and safe staging.
//!
//! These tests drive the public discovery API the way a caller such as the
//! install lifecycle does: parse a published index, fetch the artifact an entry
//! describes, verify the declared digest, and stage the result as a local
//! install source. The happy-path artifacts are the committed fixtures under
//! `tests/fixtures/discovery`, whose digests the test index declares the way a
//! real publisher would.

use std::{fs, path::Path};

use sha2::{Digest as _, Sha256};
use skillctl::discovery::{
    ArtifactType, DiscoveryEntry, DiscoveryTransport, fetch_and_stage_artifact, fetch_artifact,
    parse_index,
};
use skillctl::error::AppError;
use url::Url;

const SUPPORTED_SCHEMA: &str = "https://schemas.agentskills.io/discovery/0.2.0/schema.json";
const INDEX_URL: &str = "https://skills.example.com/.well-known/agent-skills/index.json";

/// Digest of the committed `code-review-SKILL.md` fixture.
const SKILL_MD_DIGEST: &str =
    "sha256:f6dd0259f984be74d2e54a9ecfb00c68b9f5e5d8288d2e2d20552cf764b90840";
/// Digest of the committed `code-review.tar.gz` fixture.
const TAR_GZ_DIGEST: &str =
    "sha256:8d1ebfe40553c74dd17a77b450face2c759f394fc84ebb5f33aa2e5fb2a8fff4";
/// Digest of the committed `code-review.zip` fixture.
const ZIP_DIGEST: &str = "sha256:c0caadb7b04227bd0a12e5a740d3aa262fe5e6b5ab0b4832337db81d2f1eb0b7";

/// Serve one artifact per full URL, or fail the way an origin does.
///
/// Matching the full URL lets a test declare an entry on another origin and
/// observe whether the artifact URL was fetched at all.
struct FixtureTransport {
    artifacts: Vec<(String, Result<Vec<u8>, String>)>,
}

impl FixtureTransport {
    fn serving(url: &str, artifact: &str) -> Self {
        let bytes = fs::read(fixture_path(artifact)).expect("fixture artifact is readable");
        Self {
            artifacts: vec![(url.to_string(), Ok(bytes))],
        }
    }

    fn lookup(&self, url: &Url) -> Result<Vec<u8>, AppError> {
        for (served_url, body) in &self.artifacts {
            if url.as_str() == served_url {
                return match body {
                    Ok(bytes) => Ok(bytes.clone()),
                    Err(message) => Err(AppError::DiscoveryFetch {
                        url: url.to_string(),
                        message: message.clone(),
                    }),
                };
            }
        }

        Err(AppError::DiscoveryFetch {
            url: url.to_string(),
            message: "origin responded with HTTP 404".to_string(),
        })
    }
}

impl DiscoveryTransport for FixtureTransport {
    fn fetch(&self, url: &Url) -> Result<Vec<u8>, AppError> {
        self.lookup(url)
    }

    fn fetch_artifact(&self, url: &Url) -> Result<Vec<u8>, AppError> {
        self.lookup(url)
    }
}

/// Absolute path of one committed fixture.
fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/discovery")
        .join(name)
}

/// Build a one-entry index around one declared artifact and return its entry.
fn index_entry(
    name: &str,
    artifact_type: &str,
    declared_url: &str,
    digest: &str,
) -> DiscoveryEntry {
    let document = format!(
        "{{\"$schema\":\"{SUPPORTED_SCHEMA}\",\"skills\":[{{\"name\":\"{name}\",\
         \"type\":\"{artifact_type}\",\"description\":\"Review code.\",\
         \"url\":\"{declared_url}\",\"digest\":\"{digest}\"}}]}}"
    );
    let index = parse_index(&document, &Url::parse(INDEX_URL).expect("index url parses"))
        .expect("test index parses");
    index
        .entries
        .into_iter()
        .next()
        .expect("test index declares one entry")
}

#[test]
fn a_skill_md_entry_fetches_verifies_and_stages() {
    let entry = index_entry(
        "code-review",
        "skill-md",
        "code-review/SKILL.md",
        SKILL_MD_DIGEST,
    );
    let transport = FixtureTransport::serving(
        "https://skills.example.com/.well-known/agent-skills/code-review/SKILL.md",
        "code-review-SKILL.md",
    );

    let fetched = fetch_artifact(&entry, &transport).expect("verified bytes are fetched");
    assert_eq!(fetched.artifact_type, ArtifactType::SkillMd);
    assert_eq!(fetched.digest.as_str(), SKILL_MD_DIGEST);
    assert_eq!(fetched.bytes.len(), 268);

    let staged = fetch_and_stage_artifact(&entry, &transport).expect("the artifact stages");
    let manifest = fs::read_to_string(staged.root.join("skills/code-review/SKILL.md"))
        .expect("the synthetic skill directory holds the manifest");
    assert_eq!(
        manifest,
        fs::read_to_string(fixture_path("code-review-SKILL.md")).expect("fixture is readable"),
    );
}

#[test]
fn a_tar_gz_archive_entry_fetches_verifies_and_stages() {
    let entry = index_entry(
        "code-review",
        "archive",
        "/download/code-review.tar.gz",
        TAR_GZ_DIGEST,
    );
    let transport = FixtureTransport::serving(
        "https://skills.example.com/download/code-review.tar.gz",
        "code-review.tar.gz",
    );

    let staged = fetch_and_stage_artifact(&entry, &transport).expect("the archive stages");
    assert_eq!(staged.artifact_type, ArtifactType::Archive);
    assert_eq!(staged.digest.as_str(), TAR_GZ_DIGEST);
    assert!(
        staged
            .root
            .join(".agents/skills/code-review/SKILL.md")
            .is_file(),
        "the unpacked layout should be staged in place: {}",
        staged.root.display(),
    );
    assert!(
        staged
            .root
            .join(".agents/skills/code-review/references/style.md")
            .is_file(),
        "every unpacked file should be staged: {}",
        staged.root.display(),
    );
}

#[test]
fn a_zip_archive_entry_fetches_verifies_and_stages() {
    let entry = index_entry(
        "code-review",
        "archive",
        "/download/code-review.zip",
        ZIP_DIGEST,
    );
    let transport = FixtureTransport::serving(
        "https://skills.example.com/download/code-review.zip",
        "code-review.zip",
    );

    let staged = fetch_and_stage_artifact(&entry, &transport).expect("the archive stages");
    assert!(
        staged
            .root
            .join(".agents/skills/code-review/SKILL.md")
            .is_file(),
        "the unpacked layout should be staged in place: {}",
        staged.root.display(),
    );
}

#[test]
fn an_artifact_that_does_not_match_its_declared_digest_is_refused() {
    // The index advertises the tar digest while serving the SKILL.md bytes, the
    // way a stale index does.
    let entry = index_entry(
        "code-review",
        "archive",
        "/download/code-review.tar.gz",
        TAR_GZ_DIGEST,
    );
    let transport = FixtureTransport::serving(
        "https://skills.example.com/download/code-review.tar.gz",
        "code-review-SKILL.md",
    );

    let error = fetch_and_stage_artifact(&entry, &transport)
        .expect_err("unverified content is never staged");

    match &error {
        AppError::DiscoveryArtifactDigestMismatch {
            declared, actual, ..
        } => {
            assert_eq!(declared.as_str(), TAR_GZ_DIGEST);
            assert_eq!(actual.as_str(), SKILL_MD_DIGEST);
        }
        other => panic!("expected a digest mismatch, found {other:?}"),
    }
    assert_eq!(
        error.exit_status(),
        skillctl::error::ExitStatus::ValidationFailure
    );
}

#[test]
fn a_failed_artifact_download_is_an_operational_error() {
    let entry = index_entry(
        "code-review",
        "archive",
        "/download/missing.tar.gz",
        TAR_GZ_DIGEST,
    );
    let transport = FixtureTransport::serving(
        "https://skills.example.com/download/code-review.tar.gz",
        "code-review.tar.gz",
    );

    let error = fetch_and_stage_artifact(&entry, &transport).expect_err("a missing artifact fails");
    assert!(
        matches!(error, AppError::DiscoveryArtifactFetch { .. }),
        "unexpected error: {error:?}"
    );
    assert_eq!(
        error.exit_status(),
        skillctl::error::ExitStatus::OperationalError
    );
}

/// Build raw tar bytes holding `data` under a hostile entry name.
///
/// The tar writer libraries refuse to write hostile names, so the header block
/// is assembled by hand.
fn tar_with_entry(name: &str, link_name: Option<&str>, type_flag: u8, data: &[u8]) -> Vec<u8> {
    let mut header = [0_u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    header[100..108].copy_from_slice(format!("{:07o}\0", 0o644).as_bytes());
    header[108..116].copy_from_slice(b"0000000\0");
    header[116..124].copy_from_slice(b"0000000\0");
    header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
    header[136..148].copy_from_slice(b"00000000000\0");
    header[156] = type_flag;
    if let Some(link_name) = link_name {
        header[157..157 + link_name.len()].copy_from_slice(link_name.as_bytes());
    }
    header[257..262].copy_from_slice(b"ustar");
    header[263..265].copy_from_slice(b"00");
    header[148..156].copy_from_slice(b"        ");
    let checksum: u32 = header.iter().copied().map(u32::from).sum();
    header[148..156].copy_from_slice(format!("{:06o}\0 ", checksum).as_bytes());

    let mut archive = header.to_vec();
    archive.extend_from_slice(data);
    archive.extend(std::iter::repeat_n(0, (512 - data.len() % 512) % 512));
    archive.extend(std::iter::repeat_n(0, 1024));
    archive
}

#[test]
fn a_traversal_archive_is_refused_at_the_public_boundary() {
    let bytes = tar_with_entry("../escaped.txt", None, b'0', b"payload");
    let entry = index_entry(
        "code-review",
        "archive",
        "/download/hostile.tar",
        &format!("sha256:{}", sha256_hex(&bytes)),
    );
    let transport = ServingTransport {
        url: "https://skills.example.com/download/hostile.tar",
        bytes,
    };

    let error =
        fetch_and_stage_artifact(&entry, &transport).expect_err("a traversal archive is refused");

    assert!(
        matches!(error, AppError::DiscoveryArtifactRejected { .. }),
        "unexpected error: {error:?}"
    );
    assert!(
        error
            .to_string()
            .contains("escapes the extraction directory"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_symlink_archive_entry_is_refused_at_the_public_boundary() {
    let bytes = tar_with_entry("skills/link", Some("/etc/passwd"), b'2', b"");
    let entry = index_entry(
        "code-review",
        "archive",
        "/download/linked.tar",
        &format!("sha256:{}", sha256_hex(&bytes)),
    );
    let transport = ServingTransport {
        url: "https://skills.example.com/download/linked.tar",
        bytes,
    };

    let error = fetch_and_stage_artifact(&entry, &transport)
        .expect_err("a linked archive entry is refused");

    assert!(
        error.to_string().contains("symbolic or hard link"),
        "unexpected error: {error}"
    );
}

/// Transport that serves one fixed body at one full URL.
struct ServingTransport {
    url: &'static str,
    bytes: Vec<u8>,
}

impl DiscoveryTransport for ServingTransport {
    fn fetch(&self, url: &Url) -> Result<Vec<u8>, AppError> {
        self.fetch_artifact(url)
    }

    fn fetch_artifact(&self, url: &Url) -> Result<Vec<u8>, AppError> {
        if url.as_str() == self.url {
            return Ok(self.bytes.clone());
        }

        Err(AppError::DiscoveryFetch {
            url: url.to_string(),
            message: "origin responded with HTTP 404".to_string(),
        })
    }
}

#[test]
fn a_plain_http_artifact_origin_is_refused_before_any_bytes_are_read() {
    // An index read from a permitted HTTPS origin may still declare its
    // artifact on a plain-HTTP one. The transport serves digest-matching bytes
    // at that exact URL, so only the origin-trust gate can produce this
    // refusal: the digest the same document declares cannot vouch for the
    // transport the artifact arrives over.
    let entry = index_entry(
        "code-review",
        "archive",
        "http://mirror.example.com/download/code-review.tar.gz",
        TAR_GZ_DIGEST,
    );
    let transport = ServingTransport {
        url: "http://mirror.example.com/download/code-review.tar.gz",
        bytes: fs::read(fixture_path("code-review.tar.gz")).expect("fixture artifact is readable"),
    };

    let error = fetch_and_stage_artifact(&entry, &transport)
        .expect_err("an artifact origin the policy refuses is never read");

    assert!(
        matches!(error, AppError::DiscoveryValidation { .. }),
        "unexpected error: {error:?}"
    );
    assert!(
        error
            .to_string()
            .contains("origin trust policy refuses this discovery origin"),
        "the refusal names the policy: {error}"
    );
    assert_eq!(
        error.exit_status(),
        skillctl::error::ExitStatus::ValidationFailure
    );
}

/// SHA-256 of `bytes` as 64 lowercase hexadecimal characters.
///
/// The hostile archives are built in the test, so their digests are computed
/// the same way.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}
