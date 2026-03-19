#[allow(dead_code)]
mod support;

use std::{
    collections::BTreeSet,
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
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
            "skills_install".to_string(),
            "skills_remove".to_string(),
            "skills_sync".to_string(),
            "skills_update".to_string(),
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
        "cli failed: {}",
        String::from_utf8_lossy(&output.stderr)
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
