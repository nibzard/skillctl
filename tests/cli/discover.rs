use super::*;

const SUPPORTED_SCHEMA: &str = "https://schemas.agentskills.io/discovery/0.2.0/schema.json";
const INDEX_PATH: &str = "/.well-known/agent-skills/index.json";

/// Pad a seed string with `0` to the RFC's 64-character SHA-256 hex length.
fn hex64(seed: &str) -> String {
    format!("{seed:0<64}")
}

/// Build one discovery index entry as raw JSON.
fn entry(name: &str, artifact_type: &str, description: &str, url: &str, digest: &str) -> String {
    format!(
        "{{\"name\":\"{name}\",\"type\":\"{artifact_type}\",\"description\":\"{description}\",\
         \"url\":\"{url}\",\"digest\":\"{digest}\"}}"
    )
}

/// Build a discovery index document with an optional `$schema` declaration.
fn index_document(schema: Option<&str>, entries: &[String]) -> String {
    let skills = entries.join(",");
    match schema {
        Some(schema) => format!("{{\"$schema\":\"{schema}\",\"skills\":[{skills}]}}"),
        None => format!("{{\"skills\":[{skills}]}}"),
    }
}

/// An index with one relative `skill-md` URL and one path-absolute archive URL.
fn valid_index() -> String {
    index_document(
        Some(SUPPORTED_SCHEMA),
        &[
            entry(
                "code-review",
                "skill-md",
                "Review code for bugs and security issues.",
                "code-review/SKILL.md",
                &format!("sha256:{}", hex64("c4d5e6f7")),
            ),
            entry(
                "wrangler",
                "archive",
                "Deploy and manage Workers projects.",
                "/.well-known/agent-skills/wrangler.tar.gz",
                &format!("sha256:{}", hex64("a1b2c3d4")),
            ),
        ],
    )
}

fn run_discover(workspace: &TestWorkspace, argument: &str) -> assert_cmd::assert::Assert {
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "discover", argument])
        .assert()
}

fn stdout_json(assert: &assert_cmd::assert::Assert) -> Value {
    serde_json::from_slice(&assert.get_output().stdout).expect("stdout is valid json")
}

#[test]
fn discover_origin_normalizes_to_the_well_known_index_path() {
    let base = serve(vec![route(INDEX_PATH, 200, valid_index())]);
    let workspace = TestWorkspace::new();

    let assert = run_discover(&workspace, &base)
        .code(0)
        .stderr(predicate::str::is_empty());
    let body = stdout_json(&assert);

    assert_eq!(body["command"], "discover");
    assert_eq!(body["ok"], true, "unexpected response: {body:#?}");
    assert_eq!(body["warnings"], json!([]));
    assert_eq!(body["data"]["origin"], base);
    assert_eq!(body["data"]["index_url"], format!("{base}{INDEX_PATH}"));
    assert_eq!(body["data"]["schema"], SUPPORTED_SCHEMA);
    assert_eq!(body["data"]["skill_count"], 2);
}

#[test]
fn discover_accepts_a_direct_index_url() {
    let base = serve(vec![route(INDEX_PATH, 200, valid_index())]);
    let workspace = TestWorkspace::new();

    let assert = run_discover(&workspace, &format!("{base}{INDEX_PATH}"))
        .code(0)
        .stderr(predicate::str::is_empty());
    let body = stdout_json(&assert);

    assert_eq!(body["ok"], true, "unexpected response: {body:#?}");
    assert_eq!(body["data"]["index_url"], format!("{base}{INDEX_PATH}"));
    assert_eq!(body["data"]["skill_count"], 2);
}

#[test]
fn discover_resolves_relative_and_absolute_artifact_urls_against_the_index_url() {
    let base = serve(vec![route(INDEX_PATH, 200, valid_index())]);
    let workspace = TestWorkspace::new();

    let assert = run_discover(&workspace, &base).code(0);
    let body = stdout_json(&assert);
    let skills = body["data"]["skills"]
        .as_array()
        .expect("skills array exists");

    assert_eq!(skills.len(), 2, "unexpected skills payload: {skills:#?}");
    assert_eq!(skills[0]["name"], "code-review");
    assert_eq!(skills[0]["type"], "skill-md");
    assert_eq!(skills[0]["declared_url"], "code-review/SKILL.md");
    assert_eq!(
        skills[0]["url"],
        format!("{base}/.well-known/agent-skills/code-review/SKILL.md"),
    );
    assert_eq!(skills[0]["digest"], format!("sha256:{}", hex64("c4d5e6f7")));
    assert_eq!(skills[1]["name"], "wrangler");
    assert_eq!(skills[1]["type"], "archive");
    assert_eq!(
        skills[1]["url"],
        format!("{base}/.well-known/agent-skills/wrangler.tar.gz"),
    );
}

#[test]
fn discover_accepts_absolute_artifact_urls_on_other_origins() {
    let base = serve(vec![route(
        INDEX_PATH,
        200,
        index_document(
            Some(SUPPORTED_SCHEMA),
            &[entry(
                "git-workflow",
                "skill-md",
                "Follow team Git conventions.",
                "https://cdn.example.com/v2/skills/git-workflow/SKILL.md",
                &format!("sha256:{}", hex64("a7b8c9d0")),
            )],
        ),
    )]);
    let workspace = TestWorkspace::new();

    let assert = run_discover(&workspace, &base).code(0);
    let body = stdout_json(&assert);
    let skills = body["data"]["skills"]
        .as_array()
        .expect("skills array exists");

    assert_eq!(skills.len(), 1, "unexpected skills payload: {skills:#?}");
    assert_eq!(
        skills[0]["url"],
        "https://cdn.example.com/v2/skills/git-workflow/SKILL.md",
    );
}

#[test]
fn discover_reports_an_invalid_digest_as_a_validation_failure() {
    let base = serve(vec![route(
        INDEX_PATH,
        200,
        index_document(
            Some(SUPPORTED_SCHEMA),
            &[entry(
                "code-review",
                "skill-md",
                "Review code.",
                "code-review/SKILL.md",
                "sha256:not-a-digest",
            )],
        ),
    )]);
    let workspace = TestWorkspace::new();

    let assert = run_discover(&workspace, &base).code(3);
    let body = stdout_json(&assert);

    assert_eq!(body["ok"], false, "unexpected response: {body:#?}");
    assert_eq!(body["command"], "discover");
    assert!(
        body["errors"]
            .as_array()
            .expect("errors array exists")
            .iter()
            .any(|error| error.as_str().unwrap_or_default().contains("digest")),
        "unexpected errors: {body:#?}",
    );
}

#[test]
fn discover_reports_an_invalid_skill_name_as_a_validation_failure() {
    let base = serve(vec![route(
        INDEX_PATH,
        200,
        index_document(
            Some(SUPPORTED_SCHEMA),
            &[entry(
                "Code_Review",
                "skill-md",
                "Review code.",
                "code-review/SKILL.md",
                &format!("sha256:{}", hex64("c4d5e6f7")),
            )],
        ),
    )]);
    let workspace = TestWorkspace::new();

    let assert = run_discover(&workspace, &base).code(3);
    let body = stdout_json(&assert);

    assert_eq!(body["ok"], false, "unexpected response: {body:#?}");
    assert!(
        body["errors"]
            .as_array()
            .expect("errors array exists")
            .iter()
            .any(|error| error.as_str().unwrap_or_default().contains("name")),
        "unexpected errors: {body:#?}",
    );
}

#[test]
fn discover_rejects_an_unrecognized_schema_version() {
    let base = serve(vec![route(
        INDEX_PATH,
        200,
        index_document(
            Some("https://schemas.example.com/discovery/9.9.9/schema.json"),
            &[entry(
                "code-review",
                "skill-md",
                "Review code.",
                "code-review/SKILL.md",
                &format!("sha256:{}", hex64("c4d5e6f7")),
            )],
        ),
    )]);
    let workspace = TestWorkspace::new();

    let assert = run_discover(&workspace, &base).code(3);
    let body = stdout_json(&assert);

    assert_eq!(body["ok"], false, "unexpected response: {body:#?}");
    assert!(
        body["errors"]
            .as_array()
            .expect("errors array exists")
            .iter()
            .any(|error| error
                .as_str()
                .unwrap_or_default()
                .contains(SUPPORTED_SCHEMA)),
        "unexpected errors: {body:#?}",
    );
}

#[test]
fn discover_reports_a_missing_index_as_an_operational_error() {
    let base = serve(Vec::new());
    let workspace = TestWorkspace::new();

    let assert = run_discover(&workspace, &base).code(1);
    let body = stdout_json(&assert);

    assert_eq!(body["ok"], false, "unexpected response: {body:#?}");
    assert!(
        body["errors"]
            .as_array()
            .expect("errors array exists")
            .iter()
            .any(|error| error.as_str().unwrap_or_default().contains("404")),
        "unexpected errors: {body:#?}",
    );
}

#[test]
fn discover_skips_unrecognized_artifact_types_with_a_warning() {
    let base = serve(vec![route(
        INDEX_PATH,
        200,
        index_document(
            Some(SUPPORTED_SCHEMA),
            &[
                entry(
                    "legacy-bundle",
                    "directory",
                    "Unsupported distribution type.",
                    "legacy-bundle/",
                    &format!("sha256:{}", hex64("11111111")),
                ),
                entry(
                    "code-review",
                    "skill-md",
                    "Review code.",
                    "code-review/SKILL.md",
                    &format!("sha256:{}", hex64("c4d5e6f7")),
                ),
            ],
        ),
    )]);
    let workspace = TestWorkspace::new();

    let assert = run_discover(&workspace, &base).code(2);
    let body = stdout_json(&assert);
    let skills = body["data"]["skills"]
        .as_array()
        .expect("skills array exists");

    assert_eq!(body["ok"], true, "unexpected response: {body:#?}");
    assert_eq!(skills.len(), 1, "unexpected skills payload: {skills:#?}");
    assert_eq!(skills[0]["name"], "code-review");
    assert!(
        body["warnings"]
            .as_array()
            .expect("warnings array exists")
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap_or_default()
                .contains("legacy-bundle")),
        "unexpected warnings: {body:#?}",
    );
}

#[test]
fn discover_rejects_an_unsupported_scheme_without_contacting_a_server() {
    let workspace = TestWorkspace::new();

    let assert = run_discover(&workspace, "ftp://example.com")
        .code(3)
        .stderr(predicate::str::is_empty());
    let body = stdout_json(&assert);

    assert_eq!(body["ok"], false, "unexpected response: {body:#?}");
    assert!(
        body["errors"]
            .as_array()
            .expect("errors array exists")
            .iter()
            .any(|error| error.as_str().unwrap_or_default().contains("scheme")),
        "unexpected errors: {body:#?}",
    );
}

#[test]
fn discover_does_not_mutate_local_workspace_or_user_state() {
    let base = serve(vec![route(INDEX_PATH, 200, valid_index())]);
    let workspace = TestWorkspace::new();

    let assert = run_discover(&workspace, &base).code(0);
    assert_eq!(stdout_json(&assert)["ok"], true);

    assert!(
        !workspace.path().join(".agents").exists(),
        "discover must not create workspace control files",
    );
    assert!(
        !workspace.home_path().join(".skillctl").exists(),
        "discover must not touch the local state store",
    );
}

#[test]
fn discover_help_documents_usage_and_the_read_only_contract() {
    let mut cmd = Command::cargo_bin("skillctl").expect("binary exists");

    cmd.arg("discover")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Usage: skillctl discover [OPTIONS] <URL>",
        ))
        .stdout(predicate::str::contains(
            "skillctl discover https://example.com",
        ))
        .stdout(predicate::str::contains("It is read-only"));
}
