//! Read-only discovery of skills published through well-known discovery indexes,
//! plus verified acquisition of the artifacts an index describes.
//!
//! The Agent Skills Discovery RFC draft (version 0.2.0) lets a site publish the
//! skills it offers at `/.well-known/agent-skills/index.json`. This module
//! resolves a user-supplied origin or index URL, fetches that document over
//! HTTPS or HTTP, validates the declared schema version and every entry, and
//! resolves each artifact URL against the index URL the way RFC 3986 section 5
//! requires.
//!
//! Discovery never mutates local install state. No manifest, lockfile, state
//! store, import cache, or projection is read or written, and no history event
//! is recorded. [`fetch_artifact`] and its staging companion go one step
//! further: they write nothing outside a temporary staging root, which is the
//! shape the install pipeline already consumes.
//!
//! Artifact acquisition policy:
//!
//! - The declared SHA-256 digest is verified over the fetched bytes before the
//!   content is used, as the RFC requires. A mismatch is a validation failure
//!   and the bytes are discarded.
//! - `skill-md` artifacts are staged as a synthetic skill directory under the
//!   direct-skill packaging root.
//! - `archive` artifacts are unpacked in process, never through a shell tool,
//!   and every entry is checked for path traversal, unsafe links, and
//!   unreasonable sizes before it is written.
//!
//! Validation policy:
//!
//! - Entries with an unrecognized `type` are skipped with a warning, following
//!   the RFC's tolerance rule.
//! - Any other malformed entry (missing field, invalid name, unresolvable URL,
//!   malformed digest, duplicate name) is a validation failure, because later
//!   artifact verification trusts exactly these fields.
//! - Unrecognized fields are ignored, following the RFC.

use std::{io::Read, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use url::Url;

use crate::{app::AppContext, error::AppError, response::AppResponse, skill};

/// Path where every origin publishes its discovery index.
pub const WELL_KNOWN_INDEX_PATH: &str = "/.well-known/agent-skills/index.json";

/// Schema URIs this build knows how to process.
pub const SUPPORTED_SCHEMAS: &[&str] =
    &["https://schemas.agentskills.io/discovery/0.2.0/schema.json"];

/// Longest discovery index document accepted, in bytes.
const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024;

/// Longest remote discovery artifact accepted, in bytes.
///
/// A skill artifact is a `SKILL.md` file or a small bundle. Anything larger is
/// rejected before it is hashed or unpacked, which also bounds the memory one
/// fetch can hold.
const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

/// Artifact distribution type declared by a discovery index entry.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactType {
    /// A single `SKILL.md` file.
    SkillMd,
    /// A bundled archive.
    Archive,
}

impl ArtifactType {
    /// Parse the RFC's `type` field value.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "skill-md" => Some(Self::SkillMd),
            "archive" => Some(Self::Archive),
            _ => None,
        }
    }

    /// Return the RFC's `type` field value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SkillMd => "skill-md",
            Self::Archive => "archive",
        }
    }
}

/// A validated SHA-256 artifact digest in the RFC's `sha256:{hex}` format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactDigest {
    /// Full digest string, always `sha256:` plus 64 lowercase hex characters.
    value: String,
}

impl ArtifactDigest {
    /// Parse and validate a digest declared by a discovery index entry.
    pub fn parse(value: &str) -> Result<Self, String> {
        let hex = value
            .strip_prefix("sha256:")
            .ok_or_else(|| "must use the 'sha256:{hex}' format".to_string())?;
        if hex.len() != 64 {
            return Err(format!(
                "must contain 64 hexadecimal characters, found {}",
                hex.len()
            ));
        }
        if !hex
            .chars()
            .all(|character| character.is_ascii_digit() || ('a'..='f').contains(&character))
        {
            return Err("must use lowercase hexadecimal characters only".to_string());
        }

        Ok(Self {
            value: value.to_string(),
        })
    }

    /// Return the full digest string as declared.
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Return the 64 lowercase hexadecimal characters without the prefix.
    pub fn hex(&self) -> &str {
        self.value
            .strip_prefix("sha256:")
            .unwrap_or(self.value.as_str())
    }
}

/// One validated skill entry from a discovery index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryEntry {
    /// Skill name, validated against the open Agent Skills naming rule.
    pub name: String,
    /// Declared distribution type.
    pub artifact_type: ArtifactType,
    /// Declared description.
    pub description: String,
    /// Artifact URL exactly as declared.
    pub declared_url: String,
    /// Artifact URL resolved against the index URL.
    pub url: Url,
    /// Declared SHA-256 artifact digest.
    pub digest: ArtifactDigest,
}

/// A parsed and validated discovery index document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryIndex {
    /// Schema URI the index declared.
    pub schema: String,
    /// Validated entries, in declaration order.
    pub entries: Vec<DiscoveryEntry>,
    /// Warnings for entries that were skipped.
    pub skipped: Vec<String>,
}

/// The origin and index URL a discovery request resolves to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryTarget {
    /// Normalized origin, for example `https://example.com`.
    pub origin: String,
    /// URL of the index document to fetch.
    pub index_url: Url,
}

/// Resolve a user-supplied origin or index URL into the URL of the index.
///
/// An input whose path is the origin root resolves to the well-known index
/// path. Any other input with a path is used as the index URL directly. Inputs
/// without a scheme are treated as HTTPS origins.
pub fn resolve_target(raw: &str) -> Result<DiscoveryTarget, AppError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(discovery_validation(
            raw,
            "discovery source must not be empty",
        ));
    }

    let candidate = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let mut url = Url::parse(&candidate)
        .map_err(|source| discovery_validation(raw, format!("not a valid URL: {source}")))?;

    if url.host_str().is_none_or(str::is_empty) {
        return Err(discovery_validation(raw, "must include a host name"));
    }
    if !matches!(url.scheme(), "http" | "https") {
        return Err(discovery_validation(
            raw,
            format!("unsupported scheme '{}'; use http or https", url.scheme()),
        ));
    }

    let origin = url.origin().unicode_serialization();
    url.set_fragment(None);

    if url.path() == "/" {
        // Origin input: read the well-known index path this origin publishes.
        url.set_path(WELL_KNOWN_INDEX_PATH);
        url.set_query(None);
    }

    Ok(DiscoveryTarget {
        origin,
        index_url: url,
    })
}

/// Resolve an entry's declared artifact URL against the index URL.
///
/// Absolute URLs are used as declared. Path-absolute URLs resolve against the
/// index origin, and relative URLs resolve against the index URL's directory,
/// both following RFC 3986 section 5.
pub fn resolve_artifact_url(index_url: &Url, declared: &str) -> Result<Url, String> {
    let trimmed = declared.trim();
    if trimmed.is_empty() {
        return Err("must not be empty".to_string());
    }

    if let Ok(absolute) = Url::parse(trimmed)
        && absolute.has_host()
    {
        return if matches!(absolute.scheme(), "http" | "https") {
            Ok(absolute)
        } else {
            Err(format!(
                "unsupported scheme '{}'; use http or https",
                absolute.scheme()
            ))
        };
    }

    index_url
        .join(trimmed)
        .map_err(|source| format!("cannot be resolved against the index URL: {source}"))
}

/// Parse and validate a discovery index document.
///
/// The index URL is the base URI for every relative artifact URL in the
/// document, so it must be the URL the document was fetched from.
pub fn parse_index(body: &str, index_url: &Url) -> Result<DiscoveryIndex, AppError> {
    let input = index_url.as_str();
    let document: Value = serde_json::from_str(body).map_err(|source| {
        discovery_validation(input, format!("index document is not valid JSON: {source}"))
    })?;

    let schema = match document.get("$schema") {
        Some(Value::String(schema)) => schema.as_str(),
        Some(_) => {
            return Err(discovery_validation(
                input,
                "index field '$schema' must be a string URI",
            ));
        }
        None => {
            return Err(discovery_validation(
                input,
                "index is missing the '$schema' field; the RFC treats that as version 0.1.0, \
                 which this build does not support",
            ));
        }
    };
    if !SUPPORTED_SCHEMAS.contains(&schema) {
        return Err(discovery_validation(
            input,
            format!(
                "index declares schema '{schema}', which this build does not recognize; \
                 supported schemas: {}",
                SUPPORTED_SCHEMAS.join(", ")
            ),
        ));
    }

    let skills = document
        .get("skills")
        .ok_or_else(|| discovery_validation(input, "index is missing the 'skills' array"))?;
    let skills = skills.as_array().ok_or_else(|| {
        discovery_validation(
            input,
            "index field 'skills' must be an array of skill entries",
        )
    })?;

    let mut entries = Vec::with_capacity(skills.len());
    let mut skipped = Vec::new();
    let mut names = Vec::with_capacity(skills.len());

    for (position, skill_value) in skills.iter().enumerate() {
        let object = skill_value.as_object().ok_or_else(|| {
            discovery_validation(input, format!("skill entry {position} must be an object"))
        })?;
        let declared_name = string_field(object, "name", position, input)?;
        let label = format!("skill entry {position} ('{declared_name}')");

        if let Some(reason) = skill::skill_name_format_error(declared_name) {
            return Err(discovery_validation(
                input,
                format!("{label} declares an invalid name: {reason}"),
            ));
        }

        let declared_type = string_field(object, "type", position, input)?;
        let Some(artifact_type) = ArtifactType::parse(declared_type) else {
            skipped.push(format!(
                "skipped '{declared_name}': unrecognized artifact type '{declared_type}'"
            ));
            continue;
        };

        // Skipped entries are not reported, so they do not claim a name either.
        if names.contains(&declared_name) {
            return Err(discovery_validation(
                input,
                format!("{label} duplicates an earlier entry with the same name"),
            ));
        }

        let description = string_field(object, "description", position, input)?;
        if description.trim().is_empty() {
            return Err(discovery_validation(
                input,
                format!("{label} field 'description' must not be empty"),
            ));
        }
        if description.len() > 1_024 {
            return Err(discovery_validation(
                input,
                format!(
                    "{label} field 'description' must be at most 1024 characters, found {}",
                    description.len()
                ),
            ));
        }

        let declared_url = string_field(object, "url", position, input)?;
        let url = resolve_artifact_url(index_url, declared_url).map_err(|reason| {
            discovery_validation(
                input,
                format!("{label} field 'url' ('{declared_url}') is invalid: {reason}"),
            )
        })?;

        let declared_digest = string_field(object, "digest", position, input)?;
        let digest = ArtifactDigest::parse(declared_digest).map_err(|reason| {
            discovery_validation(
                input,
                format!("{label} field 'digest' ('{declared_digest}') is invalid: {reason}"),
            )
        })?;

        names.push(declared_name);
        entries.push(DiscoveryEntry {
            name: declared_name.to_string(),
            artifact_type,
            description: description.to_string(),
            declared_url: declared_url.to_string(),
            url,
            digest,
        });
    }

    Ok(DiscoveryIndex {
        schema: schema.to_string(),
        entries,
        skipped,
    })
}

/// Network access for discovery documents and artifacts.
///
/// The trait keeps fetch side effects out of the pure resolution and
/// validation logic so discovery can be tested without a network.
pub trait DiscoveryTransport {
    /// Fetch the raw bytes of the discovery index document at `url`.
    fn fetch(&self, url: &Url) -> Result<Vec<u8>, AppError>;

    /// Fetch the raw bytes of one artifact the index describes at `url`.
    fn fetch_artifact(&self, url: &Url) -> Result<Vec<u8>, AppError>;
}

/// Default HTTPS transport for discovery documents and artifacts.
#[derive(Clone, Copy, Debug)]
pub struct HttpTransport {
    /// Connect timeout in seconds.
    pub connect_timeout_seconds: u64,
    /// Overall request timeout in seconds.
    pub request_timeout_seconds: u64,
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self {
            connect_timeout_seconds: 10,
            request_timeout_seconds: 30,
        }
    }
}

impl DiscoveryTransport for HttpTransport {
    fn fetch(&self, url: &Url) -> Result<Vec<u8>, AppError> {
        self.get(url, MAX_INDEX_BYTES, "index document")
            .map_err(|message| AppError::DiscoveryFetch {
                url: url.to_string(),
                message,
            })
    }

    fn fetch_artifact(&self, url: &Url) -> Result<Vec<u8>, AppError> {
        self.get(url, MAX_ARTIFACT_BYTES, "downloaded artifact")
            .map_err(|message| AppError::DiscoveryFetch {
                url: url.to_string(),
                message,
            })
    }
}

impl HttpTransport {
    /// GET `url` and return at most `max_bytes` bytes of `what`.
    ///
    /// Reading stops one byte past the limit so an oversized body is reported
    /// instead of being silently truncated.
    fn get(&self, url: &Url, max_bytes: u64, what: &str) -> Result<Vec<u8>, String> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(self.connect_timeout_seconds))
            .timeout(Duration::from_secs(self.request_timeout_seconds))
            .user_agent(concat!("skillctl/", env!("CARGO_PKG_VERSION")))
            .build();

        let response = agent
            .get(url.as_str())
            .call()
            .map_err(fetch_failure_message)?;

        let mut body = Vec::new();
        response
            .into_reader()
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut body)
            .map_err(|source| format!("failed to read the response body: {source}"))?;
        if body.len() as u64 > max_bytes {
            return Err(format!("{what} is larger than the {max_bytes} byte limit"));
        }

        Ok(body)
    }
}

/// Describe a `ureq` failure without exposing library internals.
fn fetch_failure_message(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(status, _) => format!("origin responded with HTTP {status}"),
        ureq::Error::Transport(transport) => transport.to_string(),
    }
}

/// Typed request for `skillctl discover`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoverRequest {
    /// Origin or index URL to discover skills from.
    pub input: String,
}

impl DiscoverRequest {
    /// Create a request from a raw origin or index URL.
    pub fn new(input: impl Into<String>) -> Self {
        Self {
            input: input.into(),
        }
    }
}

/// Handle `skillctl discover` with the default HTTP transport.
pub fn handle_discover(
    _context: &AppContext,
    request: DiscoverRequest,
) -> Result<AppResponse, AppError> {
    let transport = HttpTransport::default();
    discover_with_transport(request, &transport)
}

/// Run one discovery request against an explicit transport.
///
/// Shared with tests so the full command path runs without a network.
pub fn discover_with_transport(
    request: DiscoverRequest,
    transport: &dyn DiscoveryTransport,
) -> Result<AppResponse, AppError> {
    let target = resolve_target(&request.input)?;
    // Reading an index is acquisition too, so the origin-trust policy gates it:
    // an origin the policy refuses is never read, and one it did not verify is
    // named in a warning instead of passing silently.
    let origin_trust =
        crate::trust::ensure_discovery_origin_permits_acquisition(target.index_url.as_str())?;
    let body = transport.fetch(&target.index_url)?;
    let document = String::from_utf8(body).map_err(|_| {
        discovery_validation(
            target.index_url.as_str(),
            "index document is not valid UTF-8",
        )
    })?;
    let index = parse_index(&document, &target.index_url)?;

    let skills: Vec<Value> = index
        .entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "name": entry.name,
                "type": entry.artifact_type.as_str(),
                "description": entry.description,
                "url": entry.url.as_str(),
                "declared_url": entry.declared_url,
                "digest": entry.digest.as_str(),
            })
        })
        .collect();

    let mut response = AppResponse::success("discover")
        .with_summary(format!(
            "Discovered {} skill{} at {}.",
            index.entries.len(),
            if index.entries.len() == 1 { "" } else { "s" },
            target.index_url
        ))
        .with_data(serde_json::json!({
            "origin": target.origin,
            "index_url": target.index_url.as_str(),
            "origin_trust": origin_trust,
            "schema": index.schema,
            "skill_count": index.entries.len(),
            "skills": skills,
        }));
    if let Some(warning) =
        crate::trust::discovery_origin_warning(origin_trust, target.index_url.as_str())
    {
        response = response.with_warning(warning);
    }
    for warning in &index.skipped {
        response = response.with_warning(warning.clone());
    }

    Ok(response)
}

/// One remote discovery artifact that was downloaded and digest-verified.
///
/// The bytes are only present when they hash to the digest the index declared,
/// so a [`FetchedArtifact`] is safe to stage without another integrity check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchedArtifact {
    /// Skill name the discovery index declared for this artifact.
    pub name: String,
    /// Distribution type the index declared.
    pub artifact_type: ArtifactType,
    /// URL the artifact was fetched from.
    pub url: Url,
    /// Digest the index declared, which the fetched bytes matched.
    pub digest: ArtifactDigest,
    /// Verified artifact bytes.
    pub bytes: Vec<u8>,
}

impl FetchedArtifact {
    /// Number of verified bytes.
    pub fn byte_size(&self) -> u64 {
        self.bytes.len() as u64
    }
}

/// One verified discovery artifact staged as a local install source.
///
/// The staging root holds a layout the install pipeline already detects, so
/// staging a discovered skill and staging a directory or archive source share
/// one path from here on.
#[derive(Debug)]
pub struct StagedArtifact {
    /// Skill name the discovery index declared.
    pub name: String,
    /// Distribution type that was staged.
    pub artifact_type: ArtifactType,
    /// Digest of the verified bytes, which is also the staged revision.
    pub digest: ArtifactDigest,
    /// Install root holding a layout the install pipeline detects.
    pub root: PathBuf,
    /// Keeps the staging directory alive until this value is dropped.
    pub _staging_dir: TempDir,
}

/// Download one discovery artifact and verify the digest its index declared.
///
/// The RFC requires clients to use verified content only, so a byte that does
/// not match the declared digest is never returned. The fetch itself is an
/// operational failure, while a digest mismatch is a validation failure: the
/// origin was reached, but it does not serve what its index advertises.
pub fn fetch_artifact(
    entry: &DiscoveryEntry,
    transport: &dyn DiscoveryTransport,
) -> Result<FetchedArtifact, AppError> {
    fetch_artifact_within(entry, transport, MAX_ARTIFACT_BYTES)
}

/// [`fetch_artifact`] with an explicit size ceiling, shared with the tests.
fn fetch_artifact_within(
    entry: &DiscoveryEntry,
    transport: &dyn DiscoveryTransport,
    max_bytes: u64,
) -> Result<FetchedArtifact, AppError> {
    let bytes = transport
        .fetch_artifact(&entry.url)
        .map_err(|error| match error {
            AppError::DiscoveryFetch { url, message } => AppError::DiscoveryArtifactFetch {
                skill: entry.name.clone(),
                url,
                message,
            },
            other => other,
        })?;

    if bytes.len() as u64 > max_bytes {
        return Err(AppError::DiscoveryArtifactRejected {
            skill: entry.name.clone(),
            url: entry.url.to_string(),
            message: format!(
                "artifact is {} bytes, which exceeds the {max_bytes} byte limit for a \
                 downloaded skill artifact",
                bytes.len()
            ),
        });
    }

    let actual = format!("sha256:{}", sha256_hex(&bytes));
    if actual != entry.digest.as_str() {
        return Err(AppError::DiscoveryArtifactDigestMismatch {
            skill: entry.name.clone(),
            url: entry.url.to_string(),
            declared: entry.digest.as_str().to_string(),
            actual,
        });
    }

    Ok(FetchedArtifact {
        name: entry.name.clone(),
        artifact_type: entry.artifact_type,
        url: entry.url.clone(),
        digest: entry.digest.clone(),
        bytes,
    })
}

/// Stage one verified artifact into a temporary install root.
///
/// The staging is described with [`StagedArtifact`]; see [`crate::source`] for
/// the staging rules themselves.
pub fn stage_artifact(artifact: &FetchedArtifact) -> Result<StagedArtifact, AppError> {
    crate::source::stage_discovered_artifact(artifact)
}

/// Download, verify, and stage one discovery artifact in one step.
pub fn fetch_and_stage_artifact(
    entry: &DiscoveryEntry,
    transport: &dyn DiscoveryTransport,
) -> Result<StagedArtifact, AppError> {
    let artifact = fetch_artifact(entry, transport)?;
    stage_artifact(&artifact)
}

/// What one re-read of a discovery index found for one skill.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndexLookup {
    /// The index still declares the skill, with this entry.
    Declared(DiscoveryEntry),
    /// The index no longer declares the skill.
    NotDeclared,
}

/// Re-read the discovery index at `index_url` and return the entry it now
/// declares for `skill`.
///
/// The lookup answers update checks for well-known imports: the digest the
/// index declares is the revision a move would target, exactly the way a Git
/// remote names the revision `ls-remote` reports.
pub fn lookup_index_entry(
    index_url: &str,
    skill: &str,
    transport: &dyn DiscoveryTransport,
) -> Result<IndexLookup, AppError> {
    let target = resolve_target(index_url)?;
    let body = transport.fetch(&target.index_url)?;
    let document = String::from_utf8(body).map_err(|_| {
        discovery_validation(
            target.index_url.as_str(),
            "index document is not valid UTF-8",
        )
    })?;
    let index = parse_index(&document, &target.index_url)?;

    Ok(index
        .entries
        .into_iter()
        .find(|entry| entry.name == skill)
        .map_or(IndexLookup::NotDeclared, IndexLookup::Declared))
}

/// SHA-256 over `bytes`, as 64 lowercase hexadecimal characters.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Read one required string field from a discovery index entry.
fn string_field<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
    position: usize,
    input: &str,
) -> Result<&'a str, AppError> {
    match object.get(field) {
        Some(Value::String(value)) => Ok(value.as_str()),
        Some(_) => Err(discovery_validation(
            input,
            format!("skill entry {position} field '{field}' must be a string"),
        )),
        None => Err(discovery_validation(
            input,
            format!("skill entry {position} is missing required field '{field}'"),
        )),
    }
}

fn discovery_validation(input: impl Into<String>, message: impl Into<String>) -> AppError {
    AppError::DiscoveryValidation {
        input: input.into(),
        message: message.into(),
    }
}

/// Test-only helper for building a context without touching the filesystem.
#[cfg(test)]
fn test_context() -> AppContext {
    AppContext {
        working_directory: std::path::PathBuf::from("/nonexistent-skillctl-test-workspace"),
        output_mode: crate::app::OutputMode::Json,
        verbosity: crate::app::Verbosity::Normal,
        interaction_mode: crate::app::InteractionMode::NonInteractive,
        selector: crate::app::CommandSelector::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const INDEX_URL: &str = "https://example.com/.well-known/agent-skills/index.json";

    fn index_url() -> Url {
        Url::parse(INDEX_URL).expect("index URL parses")
    }

    fn hex64(seed: &str) -> String {
        format!("{seed:0<64}")
    }

    fn valid_entry_json(name: &str, artifact_type: &str, url: &str) -> String {
        format!(
            "{{\"name\":\"{name}\",\"type\":\"{artifact_type}\",\
             \"description\":\"Does one thing well.\",\"url\":\"{url}\",\
             \"digest\":\"sha256:{}\"}}",
            hex64("c4d5e6f7")
        )
    }

    fn index_json(entries: &[String]) -> String {
        format!(
            "{{\"$schema\":\"{}\",\"skills\":[{}]}}",
            SUPPORTED_SCHEMAS[0],
            entries.join(",")
        )
    }

    /// Transport that always returns the same prepared response.
    struct FixedTransport {
        body: Result<Vec<u8>, String>,
    }

    impl DiscoveryTransport for FixedTransport {
        fn fetch(&self, _url: &Url) -> Result<Vec<u8>, AppError> {
            match &self.body {
                Ok(body) => Ok(body.clone()),
                Err(message) => Err(AppError::DiscoveryFetch {
                    url: INDEX_URL.to_string(),
                    message: message.clone(),
                }),
            }
        }

        fn fetch_artifact(&self, _url: &Url) -> Result<Vec<u8>, AppError> {
            self.fetch(_url)
        }
    }

    /// Transport that serves prepared artifact responses by URL path.
    struct ArtifactTransport {
        responses: Vec<(String, Result<Vec<u8>, String>)>,
    }

    impl ArtifactTransport {
        /// Serve `body` at `path`.
        fn new(path: &str, body: Result<Vec<u8>, String>) -> Self {
            Self {
                responses: vec![(path.to_string(), body)],
            }
        }

        fn lookup(&self, url: &Url) -> Result<Vec<u8>, AppError> {
            for (path, body) in &self.responses {
                if url.path() == path {
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

    impl DiscoveryTransport for ArtifactTransport {
        fn fetch(&self, url: &Url) -> Result<Vec<u8>, AppError> {
            self.lookup(url)
        }

        fn fetch_artifact(&self, url: &Url) -> Result<Vec<u8>, AppError> {
            self.lookup(url)
        }
    }

    /// One index entry whose digest matches `bytes`, served at `path`.
    fn artifact_entry(
        name: &str,
        artifact_type: ArtifactType,
        path: &str,
        bytes: &[u8],
    ) -> DiscoveryEntry {
        DiscoveryEntry {
            name: name.to_string(),
            artifact_type,
            description: "Does one thing well.".to_string(),
            declared_url: path.to_string(),
            url: Url::parse(&format!("https://example.com{path}")).expect("artifact url parses"),
            digest: ArtifactDigest::parse(&format!("sha256:{}", sha256_hex(bytes)))
                .expect("digest parses"),
        }
    }

    /// A `SKILL.md` body for the skill called `name`.
    fn skill_md(name: &str) -> String {
        format!("---\nname: {name}\ndescription: Does one thing well.\n---\n\n# {name}\n")
    }

    #[test]
    fn origin_input_resolves_to_the_well_known_index_path() {
        for input in [
            "https://example.com",
            "https://example.com/",
            "example.com",
            "https://example.com?tracking=1",
        ] {
            let target = resolve_target(input).expect("origin input resolves");
            assert_eq!(
                target.index_url.as_str(),
                "https://example.com/.well-known/agent-skills/index.json",
                "input '{input}' resolved to the wrong index URL",
            );
            assert_eq!(target.origin, "https://example.com");
        }
    }

    #[test]
    fn index_url_input_is_used_directly() {
        let target =
            resolve_target("https://cdn.example.com/skills/index.json?v=2").expect("url resolves");
        assert_eq!(
            target.index_url.as_str(),
            "https://cdn.example.com/skills/index.json?v=2",
        );
        assert_eq!(target.origin, "https://cdn.example.com");

        let target =
            resolve_target("http://127.0.0.1:8080/agent-skills/index.json").expect("url resolves");
        assert_eq!(
            target.index_url.as_str(),
            "http://127.0.0.1:8080/agent-skills/index.json",
        );
    }

    #[test]
    fn input_without_a_scheme_defaults_to_https() {
        let target = resolve_target("example.com/.well-known/agent-skills/index.json")
            .expect("bare host resolves");
        assert_eq!(
            target.index_url.as_str(),
            "https://example.com/.well-known/agent-skills/index.json",
        );
    }

    #[test]
    fn invalid_origin_inputs_are_rejected() {
        for input in [
            "",
            "   ",
            "ftp://example.com",
            "file:///tmp/index.json",
            "not a url",
        ] {
            let error = resolve_target(input).expect_err("input must be rejected");
            assert!(
                matches!(error, AppError::DiscoveryValidation { .. }),
                "input '{input}' produced {error:?}",
            );
        }
    }

    #[test]
    fn relative_artifact_urls_resolve_against_the_index_directory() {
        let resolved =
            resolve_artifact_url(&index_url(), "code-review/SKILL.md").expect("url resolves");
        assert_eq!(
            resolved.as_str(),
            "https://example.com/.well-known/agent-skills/code-review/SKILL.md",
        );
    }

    #[test]
    fn path_absolute_artifact_urls_resolve_against_the_origin() {
        let resolved =
            resolve_artifact_url(&index_url(), "/skills/wrangler.tar.gz").expect("url resolves");
        assert_eq!(
            resolved.as_str(),
            "https://example.com/skills/wrangler.tar.gz",
        );
    }

    #[test]
    fn absolute_artifact_urls_are_kept_and_other_schemes_are_rejected() {
        let resolved = resolve_artifact_url(
            &index_url(),
            "https://cdn.example.com/v2/git-workflow/SKILL.md",
        )
        .expect("url resolves");
        assert_eq!(
            resolved.as_str(),
            "https://cdn.example.com/v2/git-workflow/SKILL.md",
        );

        assert!(resolve_artifact_url(&index_url(), "ftp://cdn.example.com/skill.md").is_err());
        assert!(resolve_artifact_url(&index_url(), "").is_err());
        assert!(resolve_artifact_url(&index_url(), "   ").is_err());
    }

    #[test]
    fn digests_must_use_lowercase_hex_sha256_format() {
        assert!(ArtifactDigest::parse(&format!("sha256:{}", hex64("c4d5e6f7"))).is_ok());
        assert!(ArtifactDigest::parse("sha256:short").is_err());
        assert!(ArtifactDigest::parse(&format!("sha256:{}", "A".repeat(64))).is_err());
        assert!(ArtifactDigest::parse(&format!("sha512:{}", hex64("c4d5e6f7"))).is_err());
        assert!(ArtifactDigest::parse(&hex64("c4d5e6f7")).is_err());

        let digest =
            ArtifactDigest::parse(&format!("sha256:{}", hex64("c4d5e6f7"))).expect("digest parses");
        assert_eq!(digest.as_str(), format!("sha256:{}", hex64("c4d5e6f7")));
        assert_eq!(digest.hex(), hex64("c4d5e6f7"));
    }

    #[test]
    fn a_valid_index_parses_into_typed_entries() {
        let document = index_json(&[
            valid_entry_json("code-review", "skill-md", "code-review/SKILL.md"),
            valid_entry_json(
                "wrangler",
                "archive",
                "/.well-known/agent-skills/wrangler.tar.gz",
            ),
        ]);

        let index = parse_index(&document, &index_url()).expect("index parses");
        assert_eq!(index.schema, SUPPORTED_SCHEMAS[0]);
        assert_eq!(index.entries.len(), 2);
        assert!(index.skipped.is_empty());
        assert_eq!(index.entries[0].name, "code-review");
        assert_eq!(index.entries[0].artifact_type, ArtifactType::SkillMd);
        assert_eq!(
            index.entries[0].url.as_str(),
            "https://example.com/.well-known/agent-skills/code-review/SKILL.md",
        );
        assert_eq!(index.entries[1].artifact_type, ArtifactType::Archive);
    }

    #[test]
    fn an_empty_skills_array_is_a_valid_index() {
        let index = parse_index(&index_json(&[]), &index_url()).expect("index parses");
        assert!(index.entries.is_empty());
        assert!(index.skipped.is_empty());
    }

    #[test]
    fn unrecognized_artifact_types_are_skipped_with_a_warning() {
        let document = index_json(&[
            valid_entry_json("legacy", "directory", "legacy/"),
            valid_entry_json("code-review", "skill-md", "code-review/SKILL.md"),
        ]);

        let index = parse_index(&document, &index_url()).expect("index parses");
        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].name, "code-review");
        assert_eq!(
            index.skipped,
            vec!["skipped 'legacy': unrecognized artifact type 'directory'"]
        );
    }

    #[test]
    fn a_skipped_entry_does_not_claim_its_skill_name() {
        let document = index_json(&[
            valid_entry_json("code-review", "directory", "legacy/"),
            valid_entry_json("code-review", "skill-md", "code-review/SKILL.md"),
        ]);

        let index = parse_index(&document, &index_url()).expect("index parses");
        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].name, "code-review");
    }

    #[test]
    fn invalid_schema_declarations_are_rejected() {
        let missing_schema = "{\"skills\":[]}".to_string();
        let error = parse_index(&missing_schema, &index_url()).expect_err("schema is required");
        assert!(
            error.to_string().contains("$schema"),
            "unexpected error: {error}",
        );

        let non_string_schema = "{\"$schema\":7,\"skills\":[]}".to_string();
        assert!(parse_index(&non_string_schema, &index_url()).is_err());

        let unknown_schema =
            "{\"$schema\":\"https://example.com/9.9.9/schema.json\",\"skills\":[]}".to_string();
        let error = parse_index(&unknown_schema, &index_url()).expect_err("schema must be known");
        assert!(
            error.to_string().contains(SUPPORTED_SCHEMAS[0]),
            "unexpected error: {error}",
        );
    }

    #[test]
    fn malformed_documents_and_entries_are_rejected() {
        let cases: Vec<String> = vec![
            "not json".to_string(),
            "{\"$schema\":\"x\"}".to_string(),
            "{\"skills\":{}}".to_string(),
            index_json(&["7".to_string()]),
            index_json(&["{\"name\":\"code-review\"}".to_string()]),
            index_json(&[format!(
                "{{\"name\":\"Code Review\",\"type\":\"skill-md\",\
                 \"description\":\"d.\",\"url\":\"a/SKILL.md\",\"digest\":\"sha256:{}\"}}",
                hex64("c4d5e6f7")
            )]),
            index_json(&[format!(
                "{{\"name\":\"code--review\",\"type\":\"skill-md\",\
                 \"description\":\"d.\",\"url\":\"a/SKILL.md\",\"digest\":\"sha256:{}\"}}",
                hex64("c4d5e6f7")
            )]),
            index_json(&[format!(
                "{{\"name\":\"{}\",\"type\":\"skill-md\",\"description\":\"d.\",\
                 \"url\":\"a/SKILL.md\",\"digest\":\"sha256:{}\"}}",
                "a".repeat(65),
                hex64("c4d5e6f7")
            )]),
            index_json(&[
                valid_entry_json("code-review", "skill-md", "code-review/SKILL.md")
                    .replace("sha256:", "sha256!"),
            ]),
            index_json(&[
                valid_entry_json("code-review", "skill-md", "code-review/SKILL.md"),
                valid_entry_json("code-review", "skill-md", "code-review/SKILL.md"),
            ]),
            index_json(&[
                valid_entry_json("empty-description", "skill-md", "x/SKILL.md")
                    .replace("Does one thing well.", ""),
            ]),
        ];

        for case in cases {
            let error = parse_index(&case, &index_url()).expect_err("index must be rejected");
            assert!(
                matches!(error, AppError::DiscoveryValidation { .. }),
                "document {case} produced {error:?}",
            );
        }
    }

    #[test]
    fn oversized_descriptions_are_rejected() {
        let long_description = "d".repeat(1_025);
        let document = index_json(&[format!(
            "{{\"name\":\"code-review\",\"type\":\"skill-md\",\
             \"description\":\"{long_description}\",\"url\":\"a/SKILL.md\",\
             \"digest\":\"sha256:{}\"}}",
            hex64("c4d5e6f7")
        )]);

        assert!(parse_index(&document, &index_url()).is_err());
    }

    #[test]
    fn lookup_index_entry_finds_and_misses_entries_by_name() {
        let transport = FixedTransport {
            body: Ok(index_json(&[valid_entry_json(
                "code-review",
                "skill-md",
                "code-review/SKILL.md",
            )])
            .into_bytes()),
        };
        let index_url = index_url();

        let declared = lookup_index_entry(index_url.as_str(), "code-review", &transport)
            .expect("the lookup reads the index");
        match declared {
            IndexLookup::Declared(entry) => {
                assert_eq!(entry.name, "code-review");
                assert_eq!(
                    entry.url.as_str(),
                    "https://example.com/.well-known/agent-skills/code-review/SKILL.md"
                );
            }
            other => panic!("the entry is declared: {other:?}"),
        }

        let missing = lookup_index_entry(index_url.as_str(), "release-notes", &transport)
            .expect("the lookup reads the index");
        assert_eq!(missing, IndexLookup::NotDeclared);
    }

    #[test]
    fn discover_reports_entries_and_skips_through_the_shared_response_shape() {
        let transport = FixedTransport {
            body: Ok(index_json(&[
                valid_entry_json("code-review", "skill-md", "code-review/SKILL.md"),
                valid_entry_json("legacy", "directory", "legacy/"),
            ])
            .into_bytes()),
        };

        let response =
            discover_with_transport(DiscoverRequest::new("https://example.com"), &transport)
                .expect("discovery succeeds");

        assert!(response.ok);
        assert_eq!(response.command, "discover");
        // The origin is HTTPS but not a verified publisher, so the run warns
        // about it next to the entry it skipped.
        assert_eq!(response.warnings.len(), 2);
        assert!(
            response
                .warnings
                .iter()
                .any(|warning| warning.contains("not on the verified public publisher list"))
        );
        assert!(
            response
                .warnings
                .iter()
                .any(|warning| warning.contains("skipped 'legacy'"))
        );
        assert_eq!(response.data["origin"], "https://example.com");
        assert_eq!(response.data["index_url"], INDEX_URL);
        assert_eq!(response.data["origin_trust"], "unverified-remote");
        assert_eq!(response.data["skill_count"], 1);
        assert_eq!(response.data["skills"][0]["name"], "code-review");
        assert_eq!(response.data["skills"][0]["type"], "skill-md");
        assert_eq!(
            response.data["skills"][0]["url"],
            "https://example.com/.well-known/agent-skills/code-review/SKILL.md",
        );
    }

    #[test]
    fn discover_maps_fetch_failures_to_operational_errors() {
        let transport = FixedTransport {
            body: Err("origin responded with HTTP 404".to_string()),
        };

        let error =
            discover_with_transport(DiscoverRequest::new("https://example.com"), &transport)
                .expect_err("fetch failure surfaces");

        assert!(matches!(error, AppError::DiscoveryFetch { .. }));
        assert_eq!(
            error.exit_status(),
            crate::error::ExitStatus::OperationalError
        );
    }

    #[test]
    fn handle_discover_rejects_invalid_input_before_any_network_access() {
        let error = handle_discover(&test_context(), DiscoverRequest::new("ftp://example.com"))
            .expect_err("unsupported scheme is rejected");

        assert!(matches!(error, AppError::DiscoveryValidation { .. }));
        assert_eq!(
            error.exit_status(),
            crate::error::ExitStatus::ValidationFailure
        );
    }

    #[test]
    fn fetch_artifact_returns_bytes_that_match_the_declared_digest() {
        let bytes = skill_md("code-review").into_bytes();
        let entry = artifact_entry(
            "code-review",
            ArtifactType::SkillMd,
            "/artifacts/SKILL.md",
            &bytes,
        );
        let transport = ArtifactTransport::new("/artifacts/SKILL.md", Ok(bytes.clone()));

        let fetched = fetch_artifact(&entry, &transport).expect("verified artifact is returned");

        assert_eq!(fetched.name, "code-review");
        assert_eq!(fetched.artifact_type, ArtifactType::SkillMd);
        assert_eq!(fetched.byte_size(), bytes.len() as u64);
        assert_eq!(fetched.bytes, bytes);
        assert_eq!(fetched.digest.as_str(), entry.digest.as_str());
    }

    #[test]
    fn fetch_artifact_rejects_bytes_that_do_not_match_the_declared_digest() {
        let served = skill_md("code-review").into_bytes();
        let mut entry = artifact_entry(
            "code-review",
            ArtifactType::SkillMd,
            "/artifacts/SKILL.md",
            &served,
        );
        // A digest declared for other bytes, the way a stale index does.
        entry.digest =
            ArtifactDigest::parse(&format!("sha256:{}", hex64("0f1e2d3c"))).expect("digest parses");
        let transport = ArtifactTransport::new("/artifacts/SKILL.md", Ok(served.clone()));

        let error =
            fetch_artifact(&entry, &transport).expect_err("a digest mismatch refuses the bytes");

        assert!(
            matches!(error, AppError::DiscoveryArtifactDigestMismatch { .. }),
            "unexpected error: {error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains(entry.digest.as_str()),
            "the declared digest should be named: {message}"
        );
        assert!(
            message.contains(&format!("sha256:{}", sha256_hex(&served))),
            "the digest of the fetched bytes should be named: {message}"
        );
        assert!(
            message.contains("was not used"),
            "the refusal should say the content was not used: {message}"
        );
        assert_eq!(
            error.exit_status(),
            crate::error::ExitStatus::ValidationFailure
        );
    }

    #[test]
    fn fetch_artifact_rejects_an_artifact_past_the_size_ceiling() {
        let served = vec![7_u8; 32];
        let entry = artifact_entry(
            "demo",
            ArtifactType::Archive,
            "/artifacts/demo.tar.gz",
            &served,
        );
        let transport = ArtifactTransport::new("/artifacts/demo.tar.gz", Ok(served));

        let error = fetch_artifact_within(&entry, &transport, 8)
            .expect_err("an oversized artifact is refused before it is used");

        assert!(
            matches!(error, AppError::DiscoveryArtifactRejected { .. }),
            "unexpected error: {error:?}"
        );
        assert!(
            error.to_string().contains("exceeds the 8 byte limit"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn fetch_artifact_reports_a_failed_download_as_an_operational_error() {
        let bytes = skill_md("code-review").into_bytes();
        let entry = artifact_entry(
            "code-review",
            ArtifactType::SkillMd,
            "/artifacts/SKILL.md",
            &bytes,
        );
        let transport = ArtifactTransport::new(
            "/artifacts/SKILL.md",
            Err("origin responded with HTTP 500".to_string()),
        );

        let error = fetch_artifact(&entry, &transport).expect_err("a failed download surfaces");

        assert!(
            matches!(error, AppError::DiscoveryArtifactFetch { .. }),
            "unexpected error: {error:?}"
        );
        assert!(
            error.to_string().contains("code-review"),
            "the failure should name the skill it was fetching: {error}"
        );
        assert_eq!(
            error.exit_status(),
            crate::error::ExitStatus::OperationalError
        );
    }

    #[test]
    fn fetch_and_stage_stages_a_skill_md_entry_into_the_install_layout() {
        let bytes = skill_md("code-review").into_bytes();
        let entry = artifact_entry(
            "code-review",
            ArtifactType::SkillMd,
            "/artifacts/SKILL.md",
            &bytes,
        );
        let transport = ArtifactTransport::new("/artifacts/SKILL.md", Ok(bytes.clone()));

        let staged =
            fetch_and_stage_artifact(&entry, &transport).expect("a verified skill-md entry stages");

        assert_eq!(staged.name, "code-review");
        assert_eq!(staged.artifact_type, ArtifactType::SkillMd);
        assert_eq!(
            fs::read_to_string(staged.root.join("skills/code-review/SKILL.md"))
                .expect("the staged manifest exists"),
            String::from_utf8_lossy(&bytes),
        );
    }

    #[test]
    fn fetch_and_stage_stages_an_archive_entry_into_the_install_layout() {
        use crate::source::test_archives::{self, RawTarEntry};

        let bytes = test_archives::tar_gzip_bytes(&[
            RawTarEntry::directory(".agents"),
            RawTarEntry::directory(".agents/skills"),
            RawTarEntry::directory(".agents/skills/code-review"),
            RawTarEntry::file(
                ".agents/skills/code-review/SKILL.md",
                &skill_md("code-review").into_bytes(),
            ),
        ]);
        let entry = artifact_entry(
            "code-review",
            ArtifactType::Archive,
            "/wrangler.tar.gz",
            &bytes,
        );
        let transport = ArtifactTransport::new("/wrangler.tar.gz", Ok(bytes));

        let staged =
            fetch_and_stage_artifact(&entry, &transport).expect("a verified archive stages");

        assert_eq!(staged.artifact_type, ArtifactType::Archive);
        assert!(
            staged
                .root
                .join(".agents/skills/code-review/SKILL.md")
                .is_file(),
            "the staged root should carry the unpacked layout: {}",
            staged.root.display()
        );
    }

    #[test]
    fn fetch_and_stage_refuses_an_archive_that_tries_to_escape_its_staging() {
        use crate::source::test_archives::{self, RawTarEntry};

        let bytes = test_archives::tar_gzip_bytes(&[
            RawTarEntry::file(
                ".agents/skills/code-review/SKILL.md",
                &skill_md("code-review").into_bytes(),
            ),
            RawTarEntry::file("../escaped.txt", b"payload"),
        ]);
        let entry = artifact_entry(
            "code-review",
            ArtifactType::Archive,
            "/wrangler.tar.gz",
            &bytes,
        );
        let transport = ArtifactTransport::new("/wrangler.tar.gz", Ok(bytes));

        let error = fetch_and_stage_artifact(&entry, &transport)
            .expect_err("a traversal archive refuses staging");

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
}
