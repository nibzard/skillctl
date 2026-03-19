//! MCP bridge domain entry points.

use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    app::{AppContext, InteractionMode, OutputMode, Verbosity},
    cli::{
        Command, InstallArgs, McpCommand, OptionalSkillArg, RollbackArgs, Scope, SkillArg,
        TelemetryCommand,
    },
    error::AppError,
    response::AppResponse,
    runtime,
};

const JSONRPC_VERSION: &str = "2.0";
const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;

const MCP_PROTOCOL_2025_03_26: McpProtocolVersion = McpProtocolVersion::V20250326;
const MCP_PROTOCOL_2025_06_18: McpProtocolVersion = McpProtocolVersion::V20250618;
const MCP_PROTOCOL_2025_11_25: McpProtocolVersion = McpProtocolVersion::V20251125;

/// Run the stdio MCP server for `skillctl`.
pub fn serve(context: &AppContext) -> Result<(), AppError> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    let mut server = McpServer::new(context.clone());

    loop {
        let mut line = String::new();
        let bytes = input
            .read_line(&mut line)
            .map_err(|source| AppError::FilesystemOperation {
                action: "read MCP stdin",
                path: PathBuf::from("<stdin>"),
                source,
            })?;
        if bytes == 0 {
            break;
        }

        let message = line.trim();
        if message.is_empty() {
            continue;
        }

        if let Some(response) = server.handle_line(message) {
            writeln!(
                output,
                "{}",
                serde_json::to_string(&response).map_err(AppError::from)?
            )
            .map_err(|source| AppError::FilesystemOperation {
                action: "write MCP stdout",
                path: PathBuf::from("<stdout>"),
                source,
            })?;
            output
                .flush()
                .map_err(|source| AppError::FilesystemOperation {
                    action: "flush MCP stdout",
                    path: PathBuf::from("<stdout>"),
                    source,
                })?;
        }
    }

    Ok(())
}

/// Handle the `skillctl mcp` command family.
pub fn handle_command(
    _context: &AppContext,
    command: &McpCommand,
) -> Result<AppResponse, AppError> {
    match command {
        McpCommand::Serve => Ok(AppResponse::success("mcp-serve")),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum McpProtocolVersion {
    V20250326,
    V20250618,
    V20251125,
}

impl McpProtocolVersion {
    const fn as_str(self) -> &'static str {
        match self {
            Self::V20250326 => "2025-03-26",
            Self::V20250618 => "2025-06-18",
            Self::V20251125 => "2025-11-25",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "2025-03-26" => Some(MCP_PROTOCOL_2025_03_26),
            "2025-06-18" => Some(MCP_PROTOCOL_2025_06_18),
            "2025-11-25" => Some(MCP_PROTOCOL_2025_11_25),
            _ => None,
        }
    }

    const fn latest() -> Self {
        MCP_PROTOCOL_2025_11_25
    }

    const fn supports_structured_output(self) -> bool {
        !matches!(self, Self::V20250326)
    }
}

struct McpServer {
    context: AppContext,
    negotiated_version: Option<McpProtocolVersion>,
    ready: bool,
}

impl McpServer {
    fn new(context: AppContext) -> Self {
        Self {
            context,
            negotiated_version: None,
            ready: false,
        }
    }

    fn handle_line(&mut self, line: &str) -> Option<Value> {
        let parsed = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(error) => {
                return Some(error_response(
                    Value::Null,
                    PARSE_ERROR,
                    format!("invalid JSON: {error}"),
                ));
            }
        };

        let Some(message) = parsed.as_object() else {
            return Some(error_response(
                Value::Null,
                INVALID_REQUEST,
                "requests must be JSON objects".to_string(),
            ));
        };

        if parsed.get("jsonrpc") != Some(&Value::String(JSONRPC_VERSION.to_string())) {
            return Some(error_response(
                message.get("id").cloned().unwrap_or(Value::Null),
                INVALID_REQUEST,
                "jsonrpc must be '2.0'".to_string(),
            ));
        }

        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Some(error_response(
                message.get("id").cloned().unwrap_or(Value::Null),
                INVALID_REQUEST,
                "request method must be a string".to_string(),
            ));
        };

        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        match self.handle_message(method, id.clone(), params) {
            Ok(Some(result)) => Some(success_response(id.unwrap_or(Value::Null), result)),
            Ok(None) => None,
            Err(error) => Some(error_response(
                id.unwrap_or(Value::Null),
                error.code,
                error.message,
            )),
        }
    }

    fn handle_message(
        &mut self,
        method: &str,
        id: Option<Value>,
        params: Value,
    ) -> Result<Option<Value>, RpcError> {
        match method {
            "initialize" => return self.handle_initialize(id, params),
            "notifications/initialized" => {
                if self.negotiated_version.is_some() {
                    self.ready = true;
                }
                return Ok(None);
            }
            "ping" => return Ok(id.map(|_| json!({}))),
            _ => {}
        }

        if id.is_none() {
            return Ok(None);
        }

        if self.negotiated_version.is_none() || !self.ready {
            return Err(RpcError::invalid_request(
                "server is not initialized".to_string(),
            ));
        }

        match method {
            "tools/list" => Ok(Some(self.handle_tools_list(params)?)),
            "tools/call" => Ok(Some(self.handle_tools_call(params)?)),
            _ => Err(RpcError::method_not_found(format!(
                "method '{method}' is not supported"
            ))),
        }
    }

    fn handle_initialize(
        &mut self,
        id: Option<Value>,
        params: Value,
    ) -> Result<Option<Value>, RpcError> {
        let Some(_) = id else {
            return Ok(None);
        };
        if self.negotiated_version.is_some() {
            return Err(RpcError::invalid_request(
                "server is already initialized".to_string(),
            ));
        }

        let params: InitializeParams = parse_params(params)?;
        let version = McpProtocolVersion::parse(&params.protocol_version)
            .unwrap_or_else(McpProtocolVersion::latest);
        self.negotiated_version = Some(version);

        Ok(Some(json!({
            "protocolVersion": version.as_str(),
            "capabilities": {
                "tools": {
                    "listChanged": false
                }
            },
            "serverInfo": {
                "name": env!("CARGO_PKG_NAME"),
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": "Use the v1 skill lifecycle tools. Tool results mirror the CLI --json response envelope."
        })))
    }

    fn handle_tools_list(&self, params: Value) -> Result<Value, RpcError> {
        let _: EmptyParams = parse_params_or_default(params)?;
        let version = self
            .negotiated_version
            .expect("tools/list requires an initialized protocol version");

        Ok(json!({
            "tools": tool_definitions(version)
        }))
    }

    fn handle_tools_call(&self, params: Value) -> Result<Value, RpcError> {
        let params: ToolCallParams = parse_params(params)?;
        let version = self
            .negotiated_version
            .expect("tools/call requires an initialized protocol version");
        let arguments = object_arguments(params.arguments)?;
        let invocation = parse_tool_invocation(&params.name, arguments)?;
        let response = run_tool_command(&self.context, invocation);
        let structured_content = serde_json::to_value(&response)
            .map_err(|error| RpcError::internal(error.to_string()))?;
        let text = serde_json::to_string_pretty(&response)
            .map_err(|error| RpcError::internal(error.to_string()))?;

        let mut result = json!({
            "content": [
                {
                    "type": "text",
                    "text": text
                }
            ],
            "isError": !response.ok
        });
        if version.supports_structured_output() {
            result["structuredContent"] = structured_content;
        }

        Ok(result)
    }
}

#[derive(Clone, Debug)]
struct RpcError {
    code: i32,
    message: String,
}

impl RpcError {
    fn invalid_request(message: String) -> Self {
        Self {
            code: INVALID_REQUEST,
            message,
        }
    }

    fn method_not_found(message: String) -> Self {
        Self {
            code: METHOD_NOT_FOUND,
            message,
        }
    }

    fn invalid_params(message: String) -> Self {
        Self {
            code: INVALID_PARAMS,
            message,
        }
    }

    fn internal(message: String) -> Self {
        Self {
            code: INTERNAL_ERROR,
            message,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InitializeParams {
    protocol_version: String,
    #[serde(default, rename = "capabilities")]
    _capabilities: Value,
    #[serde(default, rename = "clientInfo")]
    _client_info: Option<Value>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ToolSelectors {
    skill_name: Option<String>,
    scope: Option<Scope>,
    targets: Option<Vec<String>>,
}

enum ToolInvocation {
    Command {
        command: Command,
        selectors: ToolSelectors,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallToolArgs {
    source: String,
    #[serde(default)]
    skill_name: Option<String>,
    #[serde(default)]
    scope: Option<Scope>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedSkillArgs {
    skill: String,
    #[serde(default)]
    scope: Option<Scope>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedOptionalSkillArgs {
    #[serde(default)]
    skill: Option<String>,
    #[serde(default)]
    scope: Option<Scope>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RollbackToolArgs {
    skill: String,
    version_or_commit: String,
    #[serde(default)]
    scope: Option<Scope>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetedSkillArgs {
    skill: String,
    #[serde(default)]
    scope: Option<Scope>,
    #[serde(default)]
    targets: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectorArgs {
    #[serde(default)]
    scope: Option<Scope>,
    #[serde(default)]
    targets: Option<Vec<String>>,
}

fn parse_tool_invocation(name: &str, arguments: Value) -> Result<ToolInvocation, RpcError> {
    match name {
        "skills_list" => {
            let _: EmptyParams = parse_arguments(arguments)?;
            Ok(command_invocation(Command::List, ToolSelectors::default()))
        }
        "skills_install" => {
            let args: InstallToolArgs = parse_arguments(arguments)?;
            Ok(command_invocation(
                Command::Install(InstallArgs {
                    source: args.source,
                }),
                ToolSelectors {
                    skill_name: args.skill_name,
                    scope: args.scope,
                    targets: None,
                },
            ))
        }
        "skills_remove" => {
            let args: ScopedSkillArgs = parse_arguments(arguments)?;
            Ok(command_invocation(
                Command::Remove(SkillArg { skill: args.skill }),
                ToolSelectors {
                    scope: args.scope,
                    ..ToolSelectors::default()
                },
            ))
        }
        "skills_sync" => {
            let args: SelectorArgs = parse_arguments(arguments)?;
            Ok(command_invocation(
                Command::Sync,
                ToolSelectors {
                    scope: args.scope,
                    targets: args.targets,
                    ..ToolSelectors::default()
                },
            ))
        }
        "skills_update" => {
            let args: ScopedOptionalSkillArgs = parse_arguments(arguments)?;
            Ok(command_invocation(
                Command::Update(OptionalSkillArg { skill: args.skill }),
                ToolSelectors {
                    scope: args.scope,
                    ..ToolSelectors::default()
                },
            ))
        }
        "skills_rollback" => {
            let args: RollbackToolArgs = parse_arguments(arguments)?;
            Ok(command_invocation(
                Command::Rollback(RollbackArgs {
                    skill: args.skill,
                    version_or_commit: args.version_or_commit,
                }),
                ToolSelectors {
                    scope: args.scope,
                    ..ToolSelectors::default()
                },
            ))
        }
        "skills_history" => {
            let args: ScopedOptionalSkillArgs = parse_arguments(arguments)?;
            Ok(command_invocation(
                Command::History(OptionalSkillArg { skill: args.skill }),
                ToolSelectors {
                    scope: args.scope,
                    ..ToolSelectors::default()
                },
            ))
        }
        "skills_explain" => {
            let args: TargetedSkillArgs = parse_arguments(arguments)?;
            Ok(command_invocation(
                Command::Explain(SkillArg { skill: args.skill }),
                ToolSelectors {
                    scope: args.scope,
                    targets: args.targets,
                    ..ToolSelectors::default()
                },
            ))
        }
        "skills_override_create" => {
            let args: ScopedSkillArgs = parse_arguments(arguments)?;
            Ok(command_invocation(
                Command::Override(SkillArg { skill: args.skill }),
                ToolSelectors {
                    scope: args.scope,
                    ..ToolSelectors::default()
                },
            ))
        }
        "skills_validate" => {
            let args: SelectorArgs = parse_arguments(arguments)?;
            Ok(command_invocation(
                Command::Validate,
                ToolSelectors {
                    scope: args.scope,
                    targets: args.targets,
                    ..ToolSelectors::default()
                },
            ))
        }
        "skills_doctor" => {
            let args: SelectorArgs = parse_arguments(arguments)?;
            Ok(command_invocation(
                Command::Doctor,
                ToolSelectors {
                    scope: args.scope,
                    targets: args.targets,
                    ..ToolSelectors::default()
                },
            ))
        }
        "skills_telemetry_status" => {
            let _: EmptyParams = parse_arguments(arguments)?;
            Ok(command_invocation(
                Command::Telemetry {
                    command: TelemetryCommand::Status,
                },
                ToolSelectors::default(),
            ))
        }
        _ => Err(RpcError::invalid_params(format!("unknown tool '{name}'"))),
    }
}

fn command_invocation(command: Command, selectors: ToolSelectors) -> ToolInvocation {
    ToolInvocation::Command { command, selectors }
}

fn run_tool_command(context: &AppContext, invocation: ToolInvocation) -> AppResponse {
    match invocation {
        ToolInvocation::Command { command, selectors } => {
            let invocation_context = tool_context(context, selectors);
            match runtime::execute_command(&command, &invocation_context) {
                Ok(response) => response,
                Err(error) => AppResponse::failure(command.name(), error.to_string()),
            }
        }
    }
}

fn tool_context(base: &AppContext, selectors: ToolSelectors) -> AppContext {
    let mut context = base.clone();
    context.output_mode = OutputMode::Json;
    context.verbosity = Verbosity::Normal;
    context.interaction_mode = InteractionMode::NonInteractive;

    if let Some(skill_name) = selectors.skill_name {
        context.selector.skill_name = Some(skill_name);
    }
    if let Some(scope) = selectors.scope {
        context.selector.scope = Some(scope);
    }
    if let Some(targets) = selectors.targets {
        context.selector.targets = targets;
    }

    context
}

fn parse_params<T>(params: Value) -> Result<T, RpcError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(params).map_err(|error| RpcError::invalid_params(error.to_string()))
}

fn parse_params_or_default<T>(params: Value) -> Result<T, RpcError>
where
    T: Default + for<'de> Deserialize<'de>,
{
    if params.is_null() {
        Ok(T::default())
    } else {
        parse_params(params)
    }
}

fn parse_arguments<T>(arguments: Value) -> Result<T, RpcError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(arguments).map_err(|error| RpcError::invalid_params(error.to_string()))
}

fn object_arguments(arguments: Value) -> Result<Value, RpcError> {
    match arguments {
        Value::Null => Ok(json!({})),
        Value::Object(_) => Ok(arguments),
        _ => Err(RpcError::invalid_params(
            "tool arguments must be a JSON object".to_string(),
        )),
    }
}

fn success_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result
    })
}

fn error_response(id: Value, code: i32, message: String) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

fn tool_definitions(version: McpProtocolVersion) -> Vec<Value> {
    let mut tools = vec![
        tool_definition(
            version,
            "skills_list",
            "List managed skills.",
            empty_schema(),
        ),
        tool_definition(
            version,
            "skills_install",
            "Install one skill from a Git URL, local directory, or archive.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "source": {
                        "type": "string"
                    },
                    "skill_name": {
                        "type": "string"
                    },
                    "scope": scope_schema()
                },
                "required": ["source"]
            }),
        ),
        tool_definition(
            version,
            "skills_remove",
            "Remove one managed skill.",
            required_skill_schema(),
        ),
        tool_definition(
            version,
            "skills_sync",
            "Recompute and materialize generated projections.",
            selector_schema(),
        ),
        tool_definition(
            version,
            "skills_update",
            "Check managed imported skills for updates.",
            optional_skill_schema(),
        ),
        tool_definition(
            version,
            "skills_rollback",
            "Roll back one managed skill to a recorded version or commit.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "skill": {
                        "type": "string"
                    },
                    "version_or_commit": {
                        "type": "string"
                    },
                    "scope": scope_schema()
                },
                "required": ["skill", "version_or_commit"]
            }),
        ),
        tool_definition(
            version,
            "skills_history",
            "Show local install and modification history.",
            optional_skill_schema(),
        ),
        tool_definition(
            version,
            "skills_explain",
            "Explain which skill wins and why.",
            targeted_skill_schema(),
        ),
        tool_definition(
            version,
            "skills_override_create",
            "Create or reuse an overlay for a managed skill.",
            required_skill_schema(),
        ),
        tool_definition(
            version,
            "skills_validate",
            "Validate manifests, lockfile state, skills, and overlays.",
            selector_schema(),
        ),
        tool_definition(
            version,
            "skills_doctor",
            "Diagnose missing, stale, shadowed, or conflicting skills.",
            selector_schema(),
        ),
        tool_definition(
            version,
            "skills_telemetry_status",
            "Show the effective telemetry policy and consent state.",
            empty_schema(),
        ),
    ];
    tools.shrink_to_fit();
    tools
}

fn tool_definition(
    version: McpProtocolVersion,
    name: &'static str,
    description: &'static str,
    input_schema: Value,
) -> Value {
    let mut tool = json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema
    });
    if version.supports_structured_output() {
        tool["outputSchema"] = response_schema();
    }
    tool
}

fn empty_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {}
    })
}

fn scope_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["workspace", "user"]
    })
}

fn targets_schema() -> Value {
    json!({
        "type": "array",
        "items": {
            "type": "string"
        }
    })
}

fn required_skill_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "skill": {
                "type": "string"
            },
            "scope": scope_schema()
        },
        "required": ["skill"]
    })
}

fn optional_skill_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "skill": {
                "type": "string"
            },
            "scope": scope_schema()
        }
    })
}

fn selector_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "scope": scope_schema(),
            "targets": targets_schema()
        }
    })
}

fn targeted_skill_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "skill": {
                "type": "string"
            },
            "scope": scope_schema(),
            "targets": targets_schema()
        },
        "required": ["skill"]
    })
}

fn response_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "ok": {
                "type": "boolean"
            },
            "command": {
                "type": "string"
            },
            "warnings": {
                "type": "array",
                "items": {
                    "type": "string"
                }
            },
            "errors": {
                "type": "array",
                "items": {
                    "type": "string"
                }
            },
            "data": {}
        },
        "required": ["ok", "command", "warnings", "errors", "data"]
    })
}
