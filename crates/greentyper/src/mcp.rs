//! Bounded, explicit MCP stdio discovery.

use std::error::Error;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use crate::local_process::{ProcessContainer, configure_command};

pub(crate) const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
const MCP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FRAME_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_TOOL_COUNT: usize = 128;
const MAX_TOOL_NAME_BYTES: usize = 128;
const MAX_DESCRIPTION_BYTES: usize = 2 * 1024;
const MAX_SCHEMA_BYTES: usize = 32 * 1024;
const MAX_COMMAND_BYTES: usize = 4 * 1024;
const MAX_ARGUMENT_BYTES: usize = 4 * 1024;

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct McpCommandSpec {
    pub(crate) program: String,
    pub(crate) arguments: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct McpDiscovery {
    protocol_version: String,
    tools: Vec<McpToolView>,
}

#[derive(Debug, Serialize)]
struct McpToolView {
    name: String,
    description: Option<String>,
    input_schema: Value,
}

#[derive(Debug)]
pub(crate) enum McpError {
    InvalidCommand,
    Spawn,
    Io,
    Timeout,
    FrameTooLarge,
    OutputTooLarge,
    InvalidMessage,
    UnsupportedProtocol,
    RequestFailed,
    InvalidTools,
    TooManyTools,
    InvalidTool,
    ToolSchemaTooLarge,
    PaginatedTools,
}

impl fmt::Display for McpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidCommand => "MCP command is invalid",
            Self::Spawn => "MCP server could not be started",
            Self::Io => "MCP server I/O failed",
            Self::Timeout => "MCP server timed out",
            Self::FrameTooLarge => "MCP server message exceeds the frame limit",
            Self::OutputTooLarge => "MCP server output exceeds the byte limit",
            Self::InvalidMessage => "MCP server returned an invalid JSON-RPC message",
            Self::UnsupportedProtocol => "MCP server does not support the current MCP protocol",
            Self::RequestFailed => "MCP server rejected the discovery request",
            Self::InvalidTools => "MCP server returned an invalid tools list",
            Self::TooManyTools => "MCP server returned too many tools",
            Self::InvalidTool => "MCP server returned an invalid tool",
            Self::ToolSchemaTooLarge => "MCP tool schema exceeds the byte limit",
            Self::PaginatedTools => {
                "MCP tools list is paginated and cannot be completed in one bounded request"
            }
        };
        formatter.write_str(message)
    }
}

impl Error for McpError {}

pub(crate) fn discover(spec: &McpCommandSpec) -> Result<McpDiscovery, McpError> {
    validate_command(spec)?;

    let mut command = Command::new(&spec.program);
    command
        .args(&spec.arguments)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_command(&mut command);
    let mut container = ProcessContainer::new().map_err(|_| McpError::Spawn)?;
    let mut child = command.spawn().map_err(|_| McpError::Spawn)?;
    if container.activate(&mut child).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(McpError::Spawn);
    }
    let stdout = child.stdout.take().ok_or(McpError::Spawn)?;
    let stderr = child.stderr.take().ok_or(McpError::Spawn)?;
    let (sender, receiver) = mpsc::channel();
    let stdout_reader = thread::spawn(move || read_messages(stdout, sender));
    let stderr_reader = thread::spawn(move || drain_stderr(stderr));

    let result = (|| {
        let mut stdin = child.stdin.take().ok_or(McpError::Spawn)?;
        write_request(
            &mut stdin,
            1,
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "greentyper",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )?;
        stdin.flush().map_err(|_| McpError::Io)?;
        let initialize = receive_response(&receiver, 1)?;
        write_notification(&mut stdin, "notifications/initialized", json!({}))?;
        write_request(&mut stdin, 2, "tools/list", json!({}))?;
        stdin.flush().map_err(|_| McpError::Io)?;

        let protocol_version = initialize
            .get("result")
            .and_then(|result| result.get("protocolVersion"))
            .and_then(Value::as_str)
            .filter(|version| *version == MCP_PROTOCOL_VERSION)
            .ok_or(McpError::UnsupportedProtocol)?
            .to_owned();
        let tools = receive_response(&receiver, 2)?;
        drop(stdin);
        parse_tools(&tools).map(|tools| McpDiscovery {
            protocol_version,
            tools,
        })
    })();

    stop_child(&container, &mut child);
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();
    result
}

fn validate_command(spec: &McpCommandSpec) -> Result<(), McpError> {
    if spec.program.is_empty()
        || spec.program.len() > MAX_COMMAND_BYTES
        || !Path::new(&spec.program).is_absolute()
        || spec.program.bytes().any(|byte| byte.is_ascii_control())
        || spec.arguments.iter().any(|argument| {
            argument.len() > MAX_ARGUMENT_BYTES
                || argument.bytes().any(|byte| byte.is_ascii_control())
        })
    {
        return Err(McpError::InvalidCommand);
    }
    Ok(())
}

fn write_request(
    writer: &mut impl Write,
    id: u64,
    method: &str,
    params: Value,
) -> Result<(), McpError> {
    write_json_line(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }),
    )
}

fn write_notification(
    writer: &mut impl Write,
    method: &str,
    params: Value,
) -> Result<(), McpError> {
    write_json_line(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }),
    )
}

fn write_json_line(writer: &mut impl Write, value: &Value) -> Result<(), McpError> {
    let bytes = serde_json::to_vec(value).map_err(|_| McpError::Io)?;
    if bytes.len() > MAX_FRAME_BYTES || bytes.contains(&b'\n') {
        return Err(McpError::FrameTooLarge);
    }
    writer.write_all(&bytes).map_err(|_| McpError::Io)?;
    writer.write_all(b"\n").map_err(|_| McpError::Io)
}

fn read_messages(mut stdout: impl Read + Send + 'static, sender: Sender<Result<String, McpError>>) {
    let mut reader = BufReader::new(&mut stdout);
    let mut total = 0usize;
    loop {
        let mut line = Vec::new();
        loop {
            let available = match reader.fill_buf() {
                Ok([]) => {
                    let _ = sender.send(if line.is_empty() {
                        Err(McpError::Io)
                    } else {
                        Err(McpError::InvalidMessage)
                    });
                    return;
                }
                Ok(available) => available,
                Err(_) => {
                    let _ = sender.send(Err(McpError::Io));
                    return;
                }
            };
            let take = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |position| position + 1);
            if line.len().saturating_add(take) > MAX_FRAME_BYTES {
                let _ = sender.send(Err(McpError::FrameTooLarge));
                return;
            }
            line.extend_from_slice(&available[..take]);
            reader.consume(take);
            if line.last() == Some(&b'\n') {
                break;
            }
        }
        total = total.saturating_add(line.len());
        if total > MAX_OUTPUT_BYTES {
            let _ = sender.send(Err(McpError::OutputTooLarge));
            return;
        }
        while line
            .last()
            .is_some_and(|byte| matches!(*byte, b'\n' | b'\r'))
        {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        let line = match String::from_utf8(line) {
            Ok(line) => line,
            Err(_) => {
                let _ = sender.send(Err(McpError::InvalidMessage));
                return;
            }
        };
        if sender.send(Ok(line)).is_err() {
            return;
        }
    }
}

fn drain_stderr(mut stderr: impl Read) {
    let mut buffer = [0u8; 4096];
    let mut total = 0usize;
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(bytes) => {
                total = total.saturating_add(bytes);
                if total > MAX_OUTPUT_BYTES {
                    return;
                }
            }
        }
    }
}

fn receive_response(
    receiver: &Receiver<Result<String, McpError>>,
    id: u64,
) -> Result<Value, McpError> {
    let deadline = Instant::now() + MCP_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(McpError::Timeout);
        }
        let line = receiver
            .recv_timeout(remaining)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => McpError::Timeout,
                mpsc::RecvTimeoutError::Disconnected => McpError::Io,
            })??;
        let message: Value = serde_json::from_str(&line).map_err(|_| McpError::InvalidMessage)?;
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(McpError::InvalidMessage);
        }
        if message.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if message.get("error").is_some() {
            return Err(McpError::RequestFailed);
        }
        if !message.get("result").is_some_and(Value::is_object) {
            return Err(McpError::InvalidMessage);
        }
        return Ok(message);
    }
}

fn parse_tools(message: &Value) -> Result<Vec<McpToolView>, McpError> {
    let result = message.get("result").ok_or(McpError::InvalidTools)?;
    if result
        .get("nextCursor")
        .and_then(Value::as_str)
        .is_some_and(|cursor| !cursor.is_empty())
    {
        return Err(McpError::PaginatedTools);
    }
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or(McpError::InvalidTools)?;
    if tools.len() > MAX_TOOL_COUNT {
        return Err(McpError::TooManyTools);
    }
    let mut projection = Vec::with_capacity(tools.len());
    for tool in tools {
        let object = tool.as_object().ok_or(McpError::InvalidTool)?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .ok_or(McpError::InvalidTool)?;
        validate_text(name, MAX_TOOL_NAME_BYTES).ok_or(McpError::InvalidTool)?;
        let description = object
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(description) = description.as_deref() {
            validate_text(description, MAX_DESCRIPTION_BYTES).ok_or(McpError::InvalidTool)?;
        }
        let input_schema = object
            .get("inputSchema")
            .filter(|value| value.is_object())
            .cloned()
            .ok_or(McpError::InvalidTool)?;
        let schema_bytes = serde_json::to_vec(&input_schema).map_err(|_| McpError::InvalidTool)?;
        if schema_bytes.len() > MAX_SCHEMA_BYTES {
            return Err(McpError::ToolSchemaTooLarge);
        }
        projection.push(McpToolView {
            name: name.to_owned(),
            description,
            input_schema,
        });
    }
    projection.sort_by(|left, right| left.name.cmp(&right.name));
    if projection
        .windows(2)
        .any(|pair| pair[0].name == pair[1].name)
    {
        return Err(McpError::InvalidTool);
    }
    Ok(projection)
}

fn validate_text(value: &str, max_bytes: usize) -> Option<()> {
    (!value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control))
        .then_some(())
}

fn stop_child(container: &ProcessContainer, child: &mut Child) {
    let _ = container.terminate_tree(child);
    let _ = child.wait();
}

pub(crate) fn run_fixture(mode: &str) -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let mut line = String::new();
    while stdin.read_line(&mut line)? != 0 {
        let request: Value = match serde_json::from_str(line.trim_end()) {
            Ok(request) => request,
            Err(_) => {
                line.clear();
                continue;
            }
        };
        let id = request.get("id").and_then(Value::as_u64);
        match request.get("method").and_then(Value::as_str) {
            Some("initialize") => write_fixture_response(
                &mut stdout,
                id,
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "greentyper-fixture", "version": "1"},
                }),
            )?,
            Some("tools/list") => {
                if mode == "hang" {
                    thread::sleep(Duration::from_secs(60));
                } else if mode == "malformed" {
                    stdout.write_all(b"not-json\n")?;
                    stdout.flush()?;
                } else if mode == "oversized" {
                    let huge = "x".repeat(MAX_FRAME_BYTES + 128);
                    write_fixture_response(
                        &mut stdout,
                        id,
                        json!({"tools":[{"name":"oversized","description":huge,"inputSchema":{"type":"object"}}]}),
                    )?;
                } else {
                    write_fixture_response(
                        &mut stdout,
                        id,
                        json!({
                            "tools": [
                                {"name":"echo","description":"echo text","inputSchema":{"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}},
                                {"name":"sum","description":"sum numbers","inputSchema":{"type":"object"}},
                            ]
                        }),
                    )?;
                }
            }
            _ => {}
        }
        line.clear();
    }
    Ok(())
}

fn write_fixture_response(
    writer: &mut impl Write,
    id: Option<u64>,
    result: Value,
) -> io::Result<()> {
    let Some(id) = id else {
        return Ok(());
    };
    serde_json::to_writer(
        &mut *writer,
        &json!({"jsonrpc":"2.0","id":id,"result":result}),
    )?;
    writer.write_all(b"\n")?;
    writer.flush()
}
