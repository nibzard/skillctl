#[allow(dead_code)]
mod support;

use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
};

use serde_json::{Value, json};
use support::TestWorkspace;

const MCP_PROTOCOL_VERSION: &str = "2025-11-25";

#[test]
fn mcp_server_lists_v1_tools_and_requires_explicit_destructive_parameters() {
    let workspace = TestWorkspace::new();
    let mut server = McpServer::spawn(workspace.path(), &workspace.home_path());

    let tools = server.tools_list();
    let names = tools
        .as_array()
        .expect("tools array exists")
        .iter()
        .map(|tool| {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert_eq!(tool["outputSchema"]["type"], "object");
            tool["name"].as_str().expect("tool name exists").to_string()
        })
        .collect::<BTreeSet<_>>();

    assert_eq!(
        names,
        BTreeSet::from([
            "skills_list".to_string(),
            "skills_discover".to_string(),
            "skills_install".to_string(),
            "skills_remove".to_string(),
            "skills_sync".to_string(),
            "skills_restore".to_string(),
            "skills_verify".to_string(),
            "skills_update".to_string(),
            "skills_update_apply".to_string(),
            "skills_rollback".to_string(),
            "skills_history".to_string(),
            "skills_explain".to_string(),
            "skills_override_create".to_string(),
            "skills_validate".to_string(),
            "skills_doctor".to_string(),
            "skills_telemetry_status".to_string(),
        ]),
    );

    let error = server.request(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "skills_remove",
            "arguments": {}
        }
    }));

    assert_eq!(error["error"]["code"], -32602);
    assert!(
        error["error"]["message"]
            .as_str()
            .expect("message exists")
            .contains("missing field `skill`"),
        "unexpected error: {error:#?}",
    );

    // Discovery is read-only over MCP too, but it still needs the URL that
    // names the index to read.
    let error = server.request(json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "skills_discover",
            "arguments": {}
        }
    }));

    assert_eq!(error["error"]["code"], -32602);
    assert!(
        error["error"]["message"]
            .as_str()
            .expect("message exists")
            .contains("missing field `url`"),
        "unexpected error: {error:#?}",
    );

    // A restore rewrites committed portable state unless it is frozen, so the
    // mode is an explicit argument instead of a default.
    let error = server.request(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "skills_restore",
            "arguments": {}
        }
    }));

    assert_eq!(error["error"]["code"], -32602);
    assert!(
        error["error"]["message"]
            .as_str()
            .expect("message exists")
            .contains("missing field `frozen`"),
        "unexpected error: {error:#?}",
    );

    // Verification is read-only and takes no selectors, so it accepts no
    // arguments at all.
    let error = server.request(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "skills_verify",
            "arguments": {
                "skill": "release-notes"
            }
        }
    }));

    assert_eq!(
        error["error"]["code"], -32602,
        "unexpected error: {error:#?}"
    );
}

#[test]
fn documented_v1_tool_lists_match_the_shipped_mcp_surface() {
    let workspace = TestWorkspace::new();
    let mut server = McpServer::spawn(workspace.path(), &workspace.home_path());

    let shipped = server
        .tools_list()
        .as_array()
        .expect("tools array exists")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name exists").to_string())
        .collect::<BTreeSet<_>>();
    assert!(
        shipped.contains("skills_update_apply"),
        "skills_update_apply left the shipped surface: {shipped:?}",
    );

    let help = Command::new(env!("CARGO_BIN_EXE_skillctl"))
        .args(["mcp", "--help"])
        .output()
        .expect("mcp help launches");
    assert!(help.status.success(), "mcp --help failed");
    let help_tools = documented_tool_names(&String::from_utf8_lossy(&help.stdout));
    assert_eq!(
        help_tools, shipped,
        "src/cli.rs MCP_AFTER_LONG_HELP drifted from tools/list",
    );

    let troubleshooting = read_repository_document("docs/troubleshooting.md");
    let guide_line = troubleshooting
        .lines()
        .find(|line| line.contains("call one of the shipped v1 tools"))
        .expect("troubleshooting guide names the shipped v1 tools");
    assert_eq!(
        documented_tool_names(guide_line),
        shipped,
        "docs/troubleshooting.md v1 tool list drifted from tools/list",
    );

    let spec = read_repository_document("spec.md");
    let start = spec
        .find("### 22.2 MCP tools in v1")
        .expect("spec section 22.2 exists");
    let section = &spec[start..];
    let section = match section.find("\n### ") {
        Some(end) => &section[..end],
        None => section,
    };
    assert_eq!(
        documented_tool_names(section),
        shipped,
        "spec.md section 22.2 drifted from tools/list",
    );
}

/// Collect the `skills_...` tool names a text span documents.
fn documented_tool_names(text: &str) -> BTreeSet<String> {
    text.split(|character: char| {
        !(character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_')
    })
    .filter(|token| token.starts_with("skills_"))
    .map(str::to_string)
    .collect()
}

fn read_repository_document(relative_path: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_path);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

#[test]
fn mcp_tool_calls_reuse_the_cli_json_response_contract() {
    let workspace = TestWorkspace::new();
    workspace.write_file(
        "git-source/.agents/skills/release-notes/SKILL.md",
        &skill_manifest("release-notes", "Summarize release notes."),
    );
    workspace.init_git_repo("git-source");
    let home_path = workspace.home_path();
    let repo_url = workspace.git_repo_url("git-source");

    let mut server = McpServer::spawn(workspace.path(), &home_path);

    let install = server.call_tool(
        "skills_install",
        json!({
            "source": repo_url,
            "skill_name": "release-notes",
            "scope": "workspace"
        }),
    );
    assert_eq!(install["command"], "install");
    assert_eq!(install["ok"], true);

    let cli_list = cli_json(&home_path, workspace.path(), ["list"]);
    let mcp_list = server.call_tool("skills_list", json!({}));
    assert_eq!(mcp_list, cli_list);

    let explain = server.call_tool(
        "skills_explain",
        json!({
            "skill": "release-notes"
        }),
    );
    assert_eq!(explain["command"], "explain");
    assert_eq!(explain["ok"], true);

    let history = server.call_tool(
        "skills_history",
        json!({
            "skill": "release-notes"
        }),
    );
    assert_eq!(history["command"], "history");
    assert_eq!(history["ok"], true);

    let update = server.call_tool(
        "skills_update",
        json!({
            "skill": "release-notes"
        }),
    );
    assert_eq!(update["command"], "update");
    assert_eq!(update["ok"], true);

    // An apply with no available update is a machine-readable skip.
    let apply = server.call_tool(
        "skills_update_apply",
        json!({
            "skill": "release-notes",
            "strategy": "clean"
        }),
    );
    assert_eq!(apply["command"], "update");
    assert_eq!(apply["ok"], true);
    assert_eq!(apply["data"]["strategy"], "clean");
    assert_eq!(apply["data"]["apply_skipped"][0]["skill"], "release-notes");

    let validate = server.call_tool("skills_validate", json!({}));
    assert_eq!(validate["command"], "validate");

    let doctor = server.call_tool("skills_doctor", json!({}));
    assert_eq!(doctor["command"], "doctor");

    let sync = server.call_tool("skills_sync", json!({}));
    assert_eq!(sync["command"], "sync");
    assert_eq!(sync["ok"], true);

    let cli_telemetry = cli_json(&home_path, workspace.path(), ["telemetry", "status"]);
    let telemetry = server.call_tool("skills_telemetry_status", json!({}));
    assert_eq!(telemetry, cli_telemetry);

    let initial_effective_version = install["data"]["installed"][0]["effective_version_hash"]
        .as_str()
        .expect("effective version exists")
        .to_string();
    let rollback = server.call_tool(
        "skills_rollback",
        json!({
            "skill": "release-notes",
            "version_or_commit": initial_effective_version
        }),
    );
    assert_eq!(rollback["command"], "rollback");
    assert_eq!(rollback["ok"], true);

    let override_create = server.call_tool(
        "skills_override_create",
        json!({
            "skill": "release-notes"
        }),
    );
    assert_eq!(override_create["command"], "override");
    assert_eq!(override_create["ok"], true);

    let remove = server.call_tool(
        "skills_remove",
        json!({
            "skill": "release-notes"
        }),
    );
    assert_eq!(remove["command"], "remove");
    assert_eq!(remove["ok"], true);
}

/// `skills_history` advertises a `scope` argument, so an unnamed request in a
/// workspace with ledger rows in both scopes must answer with only the
/// requested scope's entries, exactly like `--scope <s> --json history`.
#[test]
fn skills_history_honors_the_scope_selector_like_the_cli() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest_for_targets(&["codex"]);
    workspace.write_file(
        "git-source/.agents/skills/release-notes/SKILL.md",
        &skill_manifest("release-notes", "Summarize release notes."),
    );
    workspace.write_file(
        "git-source/.agents/skills/changelog/SKILL.md",
        &skill_manifest("changelog", "Summarize a changelog."),
    );
    workspace.init_git_repo("git-source");
    let home_path = workspace.home_path();
    let repo_url = workspace.git_repo_url("git-source");

    let mut server = McpServer::spawn(workspace.path(), &home_path);

    // One skill per scope leaves ledger rows in both, so only the scope
    // selector can tell the two trails apart.
    for (skill, scope) in [("release-notes", "workspace"), ("changelog", "user")] {
        let install = server.call_tool(
            "skills_install",
            json!({
                "source": repo_url,
                "skill_name": skill,
                "scope": scope
            }),
        );
        assert_eq!(install["command"], "install");
        assert_eq!(install["ok"], true, "{install:#?}");
    }

    let user_history = server.call_tool("skills_history", json!({ "scope": "user" }));
    assert_eq!(user_history["command"], "history");
    assert_eq!(user_history["ok"], true);
    assert!(
        user_history["data"]["entries"]
            .as_array()
            .expect("entries exist")
            .iter()
            .any(|entry| entry["scope"] == "user" && entry["skill_id"] == "changelog"),
        "the user-scope request must answer with the user install: {user_history:#?}",
    );
    assert!(
        user_history["data"]["entries"]
            .as_array()
            .expect("entries exist")
            .iter()
            .all(|entry| entry["scope"] != "workspace"),
        "the user-scope request must not answer with workspace rows: {user_history:#?}",
    );
    let cli_user_history = cli_json(&home_path, workspace.path(), ["--scope", "user", "history"]);
    assert_eq!(user_history, cli_user_history);

    let workspace_history = server.call_tool("skills_history", json!({ "scope": "workspace" }));
    assert_eq!(workspace_history["command"], "history");
    assert_eq!(workspace_history["ok"], true);
    assert!(
        workspace_history["data"]["entries"]
            .as_array()
            .expect("entries exist")
            .iter()
            .any(|entry| entry["scope"] == "workspace" && entry["skill_id"] == "release-notes"),
        "the workspace-scope request must answer with the workspace install: {workspace_history:#?}",
    );
    assert!(
        workspace_history["data"]["entries"]
            .as_array()
            .expect("entries exist")
            .iter()
            .all(|entry| entry["scope"] != "user"),
        "the workspace-scope request must not answer with user rows: {workspace_history:#?}",
    );
    let cli_workspace_history = cli_json(
        &home_path,
        workspace.path(),
        ["--scope", "workspace", "history"],
    );
    assert_eq!(workspace_history, cli_workspace_history);
}

const SUPPORTED_DISCOVERY_SCHEMA: &str =
    "https://schemas.agentskills.io/discovery/0.2.0/schema.json";
const INDEX_PATH: &str = "/.well-known/agent-skills/index.json";

/// Pad a seed string with `0` to the RFC's 64-character SHA-256 hex length.
fn hex64(seed: &str) -> String {
    format!("{seed:0<64}")
}

/// Build one discovery index entry as raw JSON.
fn discovery_entry(
    name: &str,
    artifact_type: &str,
    description: &str,
    url: &str,
    digest: &str,
) -> String {
    format!(
        "{{\"name\":\"{name}\",\"type\":\"{artifact_type}\",\"description\":\"{description}\",\
         \"url\":\"{url}\",\"digest\":\"{digest}\"}}"
    )
}

/// Build a discovery index document that declares the supported schema.
fn discovery_index(entries: &[String]) -> String {
    format!(
        "{{\"$schema\":\"{SUPPORTED_DISCOVERY_SCHEMA}\",\"skills\":[{}]}}",
        entries.join(",")
    )
}

/// Serve one well-known discovery index on a free loopback port.
///
/// The server lives in the test process, so it stops with the test binary and
/// needs no cleanup registration.
fn serve_discovery_index(body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test origin binds a free port");
    let address = listener
        .local_addr()
        .expect("test origin reports its address");

    thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0_u8; 8192];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or_default()
                .split('?')
                .next()
                .unwrap_or_default();

            let (status, body) = if path == INDEX_PATH {
                ("200", body.clone())
            } else {
                ("404", "{\"error\":\"not found\"}".to_string())
            };
            let response = format!(
                "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: \
                 {}\r\nConnection: close\r\n\r\n",
                if status == "200" { "OK" } else { "Not Found" },
                body.len(),
            );

            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.flush();
        }
    });

    format!("http://{address}")
}

#[test]
fn mcp_discover_matches_the_cli_json_response() {
    let workspace = TestWorkspace::new();
    let home_path = workspace.home_path();

    let clean = serve_discovery_index(discovery_index(&[
        discovery_entry(
            "code-review",
            "skill-md",
            "Review code for bugs and security issues.",
            "code-review/SKILL.md",
            &format!("sha256:{}", hex64("c4d5e6f7")),
        ),
        discovery_entry(
            "wrangler",
            "archive",
            "Deploy and manage Workers projects.",
            "/.well-known/agent-skills/wrangler.tar.gz",
            &format!("sha256:{}", hex64("a1b2c3d4")),
        ),
    ]));
    let mut server = McpServer::spawn(workspace.path(), &home_path);

    let discover = server.call_tool("skills_discover", json!({ "url": clean }));
    assert_eq!(discover["command"], "discover");
    assert_eq!(discover["ok"], true, "unexpected response: {discover:#?}");
    assert_eq!(discover["data"]["origin"], clean);
    assert_eq!(
        discover["data"]["index_url"],
        format!("{clean}{INDEX_PATH}"),
    );
    assert_eq!(discover["data"]["skill_count"], 2);
    assert_eq!(discover["warnings"], json!([]));

    // The tool returns the exact envelope the CLI emits for the same request.
    assert_eq!(
        discover,
        cli_json_for_code(&home_path, workspace.path(), ["discover", &clean], 0),
    );

    // A skipped entry stays a success that carries a warning in both views.
    let skipped = serve_discovery_index(discovery_index(&[
        discovery_entry(
            "legacy-bundle",
            "directory",
            "Unsupported distribution type.",
            "legacy-bundle/",
            &format!("sha256:{}", hex64("11111111")),
        ),
        discovery_entry(
            "code-review",
            "skill-md",
            "Review code.",
            "code-review/SKILL.md",
            &format!("sha256:{}", hex64("c4d5e6f7")),
        ),
    ]));
    let warned = server.call_tool("skills_discover", json!({ "url": skipped }));
    assert_eq!(warned["ok"], true, "unexpected response: {warned:#?}");
    assert!(
        warned["warnings"]
            .as_array()
            .expect("warnings array exists")
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap_or_default()
                .contains("legacy-bundle")),
        "unexpected warnings: {warned:#?}",
    );
    assert_eq!(
        warned,
        cli_json_for_code(&home_path, workspace.path(), ["discover", &skipped], 2),
    );

    // An invalid digest refuses identically: the CLI exits 3, MCP signals
    // isError, and both carry the same error text.
    let refused = serve_discovery_index(discovery_index(&[discovery_entry(
        "code-review",
        "skill-md",
        "Review code.",
        "code-review/SKILL.md",
        "sha256:not-a-digest",
    )]));
    let error = server.request(json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/call",
        "params": {
            "name": "skills_discover",
            "arguments": {
                "url": refused
            }
        }
    }));
    assert!(
        error.get("error").is_none(),
        "unexpected MCP error: {error:#?}",
    );
    assert_eq!(
        error["result"]["isError"], true,
        "a refused discover must signal an MCP error: {error:#?}",
    );
    let structured = &error["result"]["structuredContent"];
    assert_eq!(structured["command"], "discover");
    assert_eq!(structured["ok"], false);
    let cli = cli_json_for_code(&home_path, workspace.path(), ["discover", &refused], 3);
    assert_eq!(cli["ok"], structured["ok"]);
    assert_eq!(cli["command"], structured["command"]);
    assert_eq!(cli["errors"], structured["errors"]);
}

/// Commit the portable state one author produces, so a second checkout can
/// restore the same pinned content over MCP.
struct PortableCheckout {
    /// Checkout that installs the skill and holds the committed lockfile.
    author: TestWorkspace,
}

impl PortableCheckout {
    fn new() -> Self {
        let author = TestWorkspace::new();
        author.write_manifest_for_targets(&["claude-code"]);
        author.write_file(
            "git-source/.agents/skills/release-notes/SKILL.md",
            &skill_manifest("release-notes", "Summarize release notes."),
        );
        author.init_git_repo("git-source");

        let install = Command::new(env!("CARGO_BIN_EXE_skillctl"))
            .current_dir(author.path())
            .env("HOME", author.home_path())
            .args([
                "--no-input",
                "--name",
                "release-notes",
                "install",
                author.git_repo_url("git-source").as_str(),
            ])
            .output()
            .expect("cli launches");
        assert!(
            install.status.success(),
            "install failed: {}",
            String::from_utf8_lossy(&install.stderr),
        );

        Self { author }
    }

    /// The projection the lockfile approved, before upstream moves on.
    fn approved_projection(&self) -> String {
        fs::read_to_string(
            self.author
                .path()
                .join(".claude/skills/release-notes/SKILL.md"),
        )
        .expect("author projection exists")
    }

    fn committed_lockfile(&self) -> Vec<u8> {
        fs::read(self.author.path().join(".agents/skillctl.lock")).expect("author lockfile exists")
    }

    /// Publish a newer upstream revision the pinned content must not follow.
    fn advance_upstream(&self) {
        self.author.write_file(
            "git-source/.agents/skills/release-notes/SKILL.md",
            &skill_manifest(
                "release-notes",
                "Rewritten upstream helper that must never be restored silently.",
            ),
        );
        self.author
            .commit_all("git-source", "rewrite upstream skill");
    }

    /// A different path carrying only the declared portable state.
    fn clone_portable_state(&self) -> TestWorkspace {
        let clone = TestWorkspace::new();
        copy_portable_state(&self.author, &clone);
        clone
    }
}

/// Copy the manifest and lockfile a teammate commits into another checkout.
fn copy_portable_state(from: &TestWorkspace, to: &TestWorkspace) {
    for relative in [".agents/skillctl.yaml", ".agents/skillctl.lock"] {
        let contents = fs::read_to_string(from.path().join(relative))
            .unwrap_or_else(|error| panic!("{relative} is readable: {error}"));
        to.write_file(relative, &contents);
    }
}

#[test]
fn mcp_restore_and_verify_reproduce_a_fresh_checkout_like_the_cli() {
    let fixture = PortableCheckout::new();
    let approved_projection = fixture.approved_projection();
    let committed_lockfile = fixture.committed_lockfile();
    fixture.advance_upstream();

    let clone = fixture.clone_portable_state();
    let home_path = clone.home_path();

    let mut server = McpServer::spawn(clone.path(), &home_path);

    let restore = server.call_tool(
        "skills_restore",
        json!({
            "frozen": true
        }),
    );
    assert_eq!(restore["command"], "restore");
    assert_eq!(restore["ok"], true, "restore failed: {restore:#?}");
    assert_eq!(restore["data"]["frozen"], true);
    assert_eq!(restore["data"]["lockfile"]["changed"], false);
    assert_eq!(
        restore["data"]["imports"][0]["restored_from"],
        "source-at-locked-revision"
    );
    assert_eq!(restore["data"]["imports"][0]["verified"], true);

    // A frozen restore leaves the committed lockfile byte-for-byte unchanged
    // and projects the pinned content, not the newer upstream revision.
    assert_eq!(
        fs::read(clone.path().join(".agents/skillctl.lock")).expect("clone lockfile exists"),
        committed_lockfile,
        "a frozen restore over MCP must not rewrite the lockfile",
    );
    let projected = fs::read_to_string(clone.path().join(".claude/skills/release-notes/SKILL.md"))
        .expect("restored projection exists");
    assert_eq!(projected, approved_projection);

    let verify = server.call_tool("skills_verify", json!({}));
    assert_eq!(verify["command"], "verify");
    assert_eq!(verify["ok"], true, "verify failed: {verify:#?}");
    assert_eq!(
        verify["data"]["imports"][0]["integrity"]["status"],
        "verified"
    );

    // The read-only report carries the same envelope the CLI emits.
    let cli_verify = cli_json(&home_path, clone.path(), ["verify"]);
    assert_eq!(cli_verify, verify);

    // An explicit non-frozen restore dispatches too. The lockfile already
    // records canonical bundle digests, so there is nothing to upgrade.
    let plain = server.call_tool(
        "skills_restore",
        json!({
            "frozen": false
        }),
    );
    assert_eq!(plain["ok"], true, "plain restore failed: {plain:#?}");
    assert_eq!(plain["data"]["lockfile"]["changed"], false);
}

/// Seed a checkout whose lockfile pins overlay content the checkout does not
/// carry, which a frozen restore must refuse before it acquires anything.
fn seed_overlay_without_files(workspace: &TestWorkspace) {
    workspace.write_manifest_for_targets(&["claude-code"]);
    workspace.write_file(
        "git-source/.agents/skills/release-notes/SKILL.md",
        &skill_manifest("release-notes", "Summarize release notes."),
    );
    workspace.init_git_repo("git-source");

    let install = Command::new(env!("CARGO_BIN_EXE_skillctl"))
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--json",
            "--no-input",
            "--name",
            "release-notes",
            "install",
            workspace.git_repo_url("git-source").as_str(),
        ])
        .output()
        .expect("cli launches");
    assert!(
        install.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&install.stderr),
    );

    for pass in 0..2 {
        // The first pass creates the overlay and pins it, the second re-pins
        // the edited content the lockfile is left holding.
        if pass == 1 {
            workspace.write_file(
                ".agents/overlays/release-notes/SKILL.md",
                &skill_manifest("release-notes", "Team-tailored release notes skill."),
            );
        }
        let output = Command::new(env!("CARGO_BIN_EXE_skillctl"))
            .current_dir(workspace.path())
            .env("HOME", workspace.home_path())
            .args(["--json", "--no-input", "override", "release-notes"])
            .output()
            .expect("cli launches");
        assert!(
            output.status.success(),
            "override failed: {}{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout),
        );
    }
}

#[test]
fn mcp_restore_refusal_agrees_with_the_cli_exit_code() {
    let author = TestWorkspace::new();
    seed_overlay_without_files(&author);

    // The clone carries the manifest and the lockfile but not the overlay
    // directory, which travels with the repository rather than the lockfile.
    let clone = TestWorkspace::new();
    copy_portable_state(&author, &clone);
    let home_path = clone.home_path();

    let mut server = McpServer::spawn(clone.path(), &home_path);

    let restore = server.request(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "skills_restore",
            "arguments": {
                "frozen": true
            }
        }
    }));

    assert!(
        restore.get("error").is_none(),
        "unexpected MCP error: {restore:#?}",
    );
    assert_eq!(
        restore["result"]["isError"], true,
        "a refused restore must signal an MCP error: {restore:#?}",
    );
    let structured = &restore["result"]["structuredContent"];
    assert_eq!(structured["command"], "restore");
    assert_eq!(structured["ok"], false);
    let errors = structured["errors"]
        .as_array()
        .expect("errors exist")
        .iter()
        .map(|error| error.as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    assert!(
        errors.iter().any(|error| error.contains("release-notes")),
        "the refusal names the skill it refused: {errors:?}",
    );

    // The refusal happens before anything is acquired or projected.
    assert!(
        !clone
            .path()
            .join(".claude/skills/release-notes/SKILL.md")
            .exists(),
        "a refused restore must not materialize a projection",
    );

    // The CLI reports the same refusal with the documented exit code.
    let cli = Command::new(env!("CARGO_BIN_EXE_skillctl"))
        .current_dir(clone.path())
        .env("HOME", &home_path)
        .args(["--json", "restore", "--frozen"])
        .output()
        .expect("cli launches");
    assert_eq!(
        cli.status.code(),
        Some(3),
        "cli exit code must report the missing overlay: {}",
        String::from_utf8_lossy(&cli.stderr),
    );
    let cli_body: Value = serde_json::from_slice(&cli.stdout).expect("cli output is valid json");
    assert_eq!(cli_body["ok"], structured["ok"]);
    assert_eq!(cli_body["command"], structured["command"]);
    assert_eq!(cli_body["errors"], structured["errors"]);
}

#[test]
fn mcp_conflict_results_agree_with_cli_json_and_exit_code() {
    let workspace = TestWorkspace::new();
    seed_same_name_import_conflict(&workspace);
    let home_path = workspace.home_path();

    let mut server = McpServer::spawn(workspace.path(), &home_path);

    let explain = server.request(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "skills_explain",
            "arguments": {
                "skill": "release-notes"
            }
        }
    }));

    assert!(
        explain.get("error").is_none(),
        "unexpected MCP error: {explain:#?}",
    );
    assert_eq!(
        explain["result"]["isError"], true,
        "conflict explain must signal an MCP error: {explain:#?}",
    );
    let structured = &explain["result"]["structuredContent"];
    assert_eq!(structured["command"], "explain");
    assert_eq!(structured["ok"], false);
    assert_eq!(structured["data"]["status"], "conflict");

    let doctor = server.request(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "skills_doctor",
            "arguments": {}
        }
    }));
    assert_eq!(
        doctor["result"]["isError"], true,
        "conflict doctor must signal an MCP error: {doctor:#?}",
    );
    assert_eq!(doctor["result"]["structuredContent"]["ok"], false);

    let cli = Command::new(env!("CARGO_BIN_EXE_skillctl"))
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "explain", "release-notes"])
        .output()
        .expect("cli launches");
    assert_eq!(
        cli.status.code(),
        Some(3),
        "cli exit code must report the conflict: {}",
        String::from_utf8_lossy(&cli.stderr),
    );
    let cli_body: Value = serde_json::from_slice(&cli.stdout).expect("cli output is valid json");
    assert_eq!(cli_body["ok"], false);
    assert_eq!(cli_body["command"], structured["command"]);
    assert_eq!(cli_body["ok"], structured["ok"]);
    assert_eq!(cli_body["errors"], structured["errors"]);
    assert_eq!(cli_body["data"]["status"], structured["data"]["status"]);
}

#[test]
fn mcp_trust_gate_blocks_update_apply_like_the_cli() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest_for_targets(&["claude-code"]);
    let home_path = workspace.home_path();
    let projected_skill = workspace.path().join(".claude/skills/delta/SKILL.md");
    let pinned_revision = seed_script_bearing_import(&workspace);

    let mut server = McpServer::spawn(workspace.path(), &home_path);

    // Empty arguments select every managed skill, which is the widest an
    // apply can reach over MCP.
    let apply = server.request(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "skills_update_apply",
            "arguments": {}
        }
    }));

    assert!(
        apply.get("error").is_none(),
        "unexpected MCP error: {apply:#?}",
    );
    assert_eq!(
        apply["result"]["isError"], true,
        "a blocked apply must signal an MCP error: {apply:#?}",
    );
    let structured = &apply["result"]["structuredContent"];
    assert_eq!(structured["command"], "update");
    assert_eq!(structured["ok"], false);
    assert!(
        structured["errors"]
            .as_array()
            .expect("errors exist")
            .iter()
            .any(|error| error
                .as_str()
                .unwrap_or("")
                .contains("trust gate is blocking update apply for 'delta'")),
        "unexpected errors: {}",
        structured["errors"],
    );

    assert!(
        fs::read_to_string(&projected_skill)
            .expect("projected skill exists")
            .contains("Original body."),
        "the gate must leave the projected copy on the pinned revision",
    );
    let lockfile = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("lockfile exists");
    assert!(
        lockfile.contains(&pinned_revision),
        "the gate must leave the lockfile on the pinned revision",
    );

    // The CLI describes the same refusal with the documented exit code.
    let cli = Command::new(env!("CARGO_BIN_EXE_skillctl"))
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "update", "delta", "--apply"])
        .output()
        .expect("cli launches");
    assert_eq!(
        cli.status.code(),
        Some(4),
        "cli exit code must report the trust gate: {}",
        String::from_utf8_lossy(&cli.stderr),
    );
    let cli_body: Value = serde_json::from_slice(&cli.stdout).expect("cli output is valid json");
    assert_eq!(cli_body["ok"], structured["ok"]);
    assert_eq!(cli_body["command"], structured["command"]);
    assert_eq!(cli_body["errors"], structured["errors"]);
}

#[test]
fn mcp_trust_gate_blocks_a_repair_apply_like_the_cli() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest_for_targets(&["claude-code"]);
    let pinned_revision = seed_script_bearing_import(&workspace);
    // An unmanaged edit in the generated copy is what a repair strategy
    // resolves, and it must not open a way past the gate.
    workspace.write_file(
        ".claude/skills/delta/SKILL.md",
        &skill_manifest_with_body("delta", "Directly edited runtime copy.", "My local edit."),
    );
    let home_path = workspace.home_path();
    let projected_skill = workspace.path().join(".claude/skills/delta/SKILL.md");

    let mut server = McpServer::spawn(workspace.path(), &home_path);

    let apply = server.request(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "skills_update_apply",
            "arguments": {
                "skill": "delta",
                "strategy": "overlay"
            }
        }
    }));

    assert!(
        apply.get("error").is_none(),
        "unexpected MCP error: {apply:#?}",
    );
    assert_eq!(
        apply["result"]["isError"], true,
        "a blocked repair apply must signal an MCP error: {apply:#?}",
    );
    let structured = &apply["result"]["structuredContent"];
    assert_eq!(structured["ok"], false);
    assert!(
        structured["errors"]
            .as_array()
            .expect("errors exist")
            .iter()
            .any(|error| error
                .as_str()
                .unwrap_or("")
                .contains("trust gate is blocking update apply for 'delta'")),
        "unexpected errors: {}",
        structured["errors"],
    );

    assert!(
        fs::read_to_string(&projected_skill)
            .expect("projected skill exists")
            .contains("My local edit."),
        "the gate must leave the unmanaged edit untouched",
    );
    let lockfile = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("lockfile exists");
    assert!(
        lockfile.contains(&pinned_revision),
        "the gate must leave the lockfile on the pinned revision",
    );

    // The CLI refuses the same repair with the documented exit code.
    let cli = Command::new(env!("CARGO_BIN_EXE_skillctl"))
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args([
            "--json",
            "update",
            "delta",
            "--apply",
            "--strategy",
            "overlay",
        ])
        .output()
        .expect("cli launches");
    assert_eq!(
        cli.status.code(),
        Some(4),
        "cli exit code must report the trust gate: {}",
        String::from_utf8_lossy(&cli.stderr),
    );
    let cli_body: Value = serde_json::from_slice(&cli.stdout).expect("cli output is valid json");
    assert_eq!(cli_body["ok"], structured["ok"]);
    assert_eq!(cli_body["errors"], structured["errors"]);
}

#[test]
fn mcp_trust_gate_blocks_rollback_like_the_cli() {
    let workspace = TestWorkspace::new();
    workspace.write_manifest_for_targets(&["claude-code"]);
    let pinned_revision = seed_script_bearing_import(&workspace);
    let home_path = workspace.home_path();
    let projected_skill = workspace.path().join(".claude/skills/delta/SKILL.md");

    // Review the installed content, then apply the waiting update, so the
    // import is unreviewed again at the newer revision and the older revision
    // is a recorded rollback target.
    let review = Command::new(env!("CARGO_BIN_EXE_skillctl"))
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "review", "delta", "--approve"])
        .output()
        .expect("cli launches");
    assert_eq!(
        review.status.code(),
        Some(0),
        "the review records: {}",
        String::from_utf8_lossy(&review.stdout),
    );
    let apply = Command::new(env!("CARGO_BIN_EXE_skillctl"))
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "update", "delta", "--apply"])
        .output()
        .expect("cli launches");
    assert_eq!(
        apply.status.code(),
        Some(0),
        "the reviewed update applies: {}",
        String::from_utf8_lossy(&apply.stdout),
    );
    let applied_revision = workspace.git_head("git-source");

    let mut server = McpServer::spawn(workspace.path(), &home_path);

    let rollback = server.request(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "skills_rollback",
            "arguments": {
                "skill": "delta",
                "version_or_commit": pinned_revision
            }
        }
    }));

    assert!(
        rollback.get("error").is_none(),
        "unexpected MCP error: {rollback:#?}",
    );
    assert_eq!(
        rollback["result"]["isError"], true,
        "a blocked rollback must signal an MCP error: {rollback:#?}",
    );
    let structured = &rollback["result"]["structuredContent"];
    assert_eq!(structured["command"], "rollback");
    assert_eq!(structured["ok"], false);
    let errors = structured["errors"]
        .as_array()
        .expect("errors exist")
        .iter()
        .map(|error| error.as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    assert!(
        errors
            .iter()
            .any(|error| error.contains("trust gate is blocking rollback for 'delta'")),
        "unexpected errors: {errors:?}",
    );
    assert!(
        errors
            .iter()
            .any(|error| error.contains("'skillctl fork delta'")),
        "the refusal names the fork an MCP client can run outside the gate: {errors:?}",
    );

    assert!(
        fs::read_to_string(&projected_skill)
            .expect("projected skill exists")
            .contains("Upstream body."),
        "the gate must leave the projected copy on the applied revision",
    );
    let lockfile = fs::read_to_string(workspace.path().join(".agents/skillctl.lock"))
        .expect("lockfile exists");
    assert!(
        lockfile.contains(&applied_revision),
        "the gate must leave the lockfile on the applied revision",
    );

    // The CLI describes the same refusal with the documented exit code.
    let cli = Command::new(env!("CARGO_BIN_EXE_skillctl"))
        .current_dir(workspace.path())
        .env("HOME", &home_path)
        .args(["--json", "rollback", "delta", &pinned_revision])
        .output()
        .expect("cli launches");
    assert_eq!(
        cli.status.code(),
        Some(4),
        "cli exit code must report the trust gate: {}",
        String::from_utf8_lossy(&cli.stderr),
    );
    let cli_body: Value = serde_json::from_slice(&cli.stdout).expect("cli output is valid json");
    assert_eq!(cli_body["ok"], structured["ok"]);
    assert_eq!(cli_body["command"], structured["command"]);
    assert_eq!(cli_body["errors"], structured["errors"]);
}

/// Install one script-bearing skill from a Git source and publish a newer
/// upstream revision, leaving the import unreviewed with elevated risk.
///
/// Returns the revision the import stays pinned to.
fn seed_script_bearing_import(workspace: &TestWorkspace) -> String {
    workspace.write_file(
        "git-source/.agents/skills/delta/SKILL.md",
        &skill_manifest_with_body("delta", "Helper.", "Original body."),
    );
    workspace.write_file(
        "git-source/.agents/skills/delta/scripts/run.sh",
        "#!/bin/sh\necho original\n",
    );
    workspace.init_git_repo("git-source");

    let output = Command::new(env!("CARGO_BIN_EXE_skillctl"))
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "delta",
            "install",
            workspace.git_repo_url("git-source").as_str(),
        ])
        .output()
        .expect("cli launches");
    assert_eq!(
        output.status.code(),
        Some(2),
        "installing a script-bearing import reports a warning: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let pinned_revision = workspace.git_head("git-source");

    workspace.write_file(
        "git-source/.agents/skills/delta/SKILL.md",
        &skill_manifest_with_body("delta", "Upstream helper.", "Upstream body."),
    );
    workspace.write_file(
        "git-source/.agents/skills/delta/scripts/run.sh",
        "#!/bin/sh\necho upstream\n",
    );
    workspace.commit_all("git-source", "advance upstream");

    pinned_revision
}

fn skill_manifest_with_body(skill_name: &str, description: &str, body: &str) -> String {
    format!(
        concat!(
            "---\n",
            "name: {skill_name}\n",
            "description: {description}\n",
            "---\n",
            "\n",
            "# {skill_name}\n",
            "\n",
            "{body}\n"
        ),
        skill_name = skill_name,
        description = description,
        body = body,
    )
}

/// Install one local skill, then duplicate the managed import under two ids so
/// the resolver reports a same-name conflict for `release-notes`.
fn seed_same_name_import_conflict(workspace: &TestWorkspace) {
    workspace.write_manifest_for_targets(&["codex"]);
    workspace.write_file(
        "conflict-source/.agents/skills/release-notes/SKILL.md",
        &skill_manifest("release-notes", "Conflict source copy."),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_skillctl"))
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            "conflict-source",
        ])
        .output()
        .expect("cli launches");
    assert!(
        output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    let stored = find_stored_import(&workspace.home_path().join(".skillctl/store/imports"))
        .expect("stored import exists");
    let first = stored.parent().expect("store parent exists").join("first");
    let second = stored.parent().expect("store parent exists").join("second");
    fs::rename(&stored, &first).expect("import renamed");
    copy_tree(&first, &second);

    replace_in_file(
        &workspace.path().join(".agents/skillctl.yaml"),
        "- id: release-notes",
        "- id: first",
    );
    replace_in_file(
        &workspace.path().join(".agents/skillctl.lock"),
        "  release-notes:",
        "  first:",
    );
    duplicate_block(
        &workspace.path().join(".agents/skillctl.yaml"),
        "  - id: first",
        "    ",
        "id: first",
        "id: second",
    );
    duplicate_block(
        &workspace.path().join(".agents/skillctl.lock"),
        "  first:",
        "    ",
        "  first:",
        "  second:",
    );
}

fn find_stored_import(root: &std::path::Path) -> Option<std::path::PathBuf> {
    for entry in fs::read_dir(root).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name() == "release-notes" {
                return Some(path);
            }
            if let Some(found) = find_stored_import(&path) {
                return Some(found);
            }
        }
    }
    None
}

fn replace_in_file(path: &std::path::Path, needle: &str, replacement: &str) {
    let contents = fs::read_to_string(path).expect("file reads");
    assert!(
        contents.contains(needle),
        "expected '{needle}' in {}",
        path.display()
    );
    fs::write(path, contents.replacen(needle, replacement, 1)).expect("file writes");
}

/// Copy the block that starts with `header`, continuing while lines are indented
/// by `continuation_indent`, and insert the copy with `renamed` replaced.
fn duplicate_block(
    path: &std::path::Path,
    header: &str,
    continuation_indent: &str,
    renamed: &str,
    replacement: &str,
) {
    let contents = fs::read_to_string(path).expect("file reads");
    let mut lines = contents.lines().collect::<Vec<_>>();
    let start = lines
        .iter()
        .position(|line| line.trim_end() == header)
        .unwrap_or_else(|| panic!("block header '{header}' exists in {}", path.display()));
    let end = start
        + 1
        + lines[start + 1..]
            .iter()
            .take_while(|line| line.starts_with(continuation_indent))
            .count();
    let duplicated: Vec<String> = lines[start..end]
        .iter()
        .map(|line| line.replacen(renamed, replacement, 1))
        .collect();
    for (index, line) in duplicated.iter().enumerate() {
        lines.insert(end + index, line.as_str());
    }
    let mut output = lines.join("\n");
    output.push('\n');
    fs::write(path, output).expect("file writes");
}

fn copy_tree(source: &std::path::Path, destination: &std::path::Path) {
    fs::create_dir_all(destination).expect("destination directory exists");
    for entry in fs::read_dir(source).expect("source directory reads") {
        let entry = entry.expect("directory entry reads");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("entry type reads").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("file copies");
        }
    }
}

fn cli_json<const N: usize>(
    home_path: &std::path::Path,
    workspace_path: &std::path::Path,
    args: [&str; N],
) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_skillctl"))
        .current_dir(workspace_path)
        .env("HOME", home_path)
        .args(["--json"])
        .args(args)
        .output()
        .expect("cli launches");
    assert!(
        output.status.success(),
        "cli failed: stderr={} stdout={}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stdout).expect("cli output is valid json")
}

/// Run the CLI with `--json`, require the given exit code, and parse the
/// envelope. `skills_discover` parity spans success (0), warnings (2), and
/// validation refusal (3), so the code is an explicit expectation.
fn cli_json_for_code<const N: usize>(
    home_path: &std::path::Path,
    workspace_path: &std::path::Path,
    args: [&str; N],
    expected_code: i32,
) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_skillctl"))
        .current_dir(workspace_path)
        .env("HOME", home_path)
        .args(["--json"])
        .args(args)
        .output()
        .expect("cli launches");
    assert_eq!(
        output.status.code(),
        Some(expected_code),
        "cli exit code mismatch: stderr={} stdout={}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
    serde_json::from_slice(&output.stdout).expect("cli output is valid json")
}

fn skill_manifest(skill_name: &str, description: &str) -> String {
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
    )
}

struct McpServer {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpServer {
    fn spawn(workspace_path: &std::path::Path, home_path: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_skillctl"))
            .current_dir(workspace_path)
            .env("HOME", home_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args(["mcp", "serve"])
            .spawn()
            .expect("mcp server launches");
        let stdin = child.stdin.take().expect("stdin exists");
        let stdout = BufReader::new(child.stdout.take().expect("stdout exists"));
        let mut server = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        server.initialize();
        server
    }

    fn initialize(&mut self) {
        let response = self.request(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "skillctl-test",
                    "version": "1.0.0"
                }
            }
        }));

        assert_eq!(response["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(response["result"]["serverInfo"]["name"], "skillctl");
        assert_eq!(
            response["result"]["capabilities"]["tools"]["listChanged"],
            false
        );

        self.send(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }));
    }

    fn tools_list(&mut self) -> Value {
        let id = self.take_id();
        self.request(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/list",
            "params": {}
        }))["result"]["tools"]
            .clone()
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let id = self.take_id();
        let response = self.request(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": arguments
            }
        }));

        assert!(
            response.get("error").is_none(),
            "unexpected MCP error: {response:#?}"
        );
        assert_eq!(response["result"]["isError"], false, "{response:#?}");
        response["result"]["structuredContent"].clone()
    }

    fn request(&mut self, message: Value) -> Value {
        self.send(message);
        self.read()
    }

    fn send(&mut self, message: Value) {
        writeln!(
            self.stdin,
            "{}",
            serde_json::to_string(&message).expect("message serializes")
        )
        .expect("message written");
        self.stdin.flush().expect("stdin flushed");
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        let bytes = self.stdout.read_line(&mut line).expect("response readable");
        assert!(bytes > 0, "mcp server exited unexpectedly");
        serde_json::from_str(&line).expect("response is valid json")
    }

    fn take_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
