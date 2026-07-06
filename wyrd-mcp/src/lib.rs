//! # wyrd-mcp
//!
//! A [Model Context Protocol](https://modelcontextprotocol.io) server over
//! wyrd recordings, so AI agents (Claude Code, editors, anything MCP-aware)
//! can ask *why is this task stuck?* against a `.wyrd` file.
//!
//! Speaks the stdio transport: newline-delimited JSON-RPC 2.0 on
//! stdin/stdout. Exposes [`wyrd_core::Recording`]'s queries as tools:
//!
//! - `why_blocked` — walk a task's park → resource → holder chain, naming
//!   deadlock cycles. Auto-picks an interesting parked task if none is named.
//! - `stats` — recording span, task count, poll percentiles, longest parks,
//!   channel depths.
//! - `world_state` — every task and resource with its status at an instant.
//!
//! The protocol layer is deliberately hand-rolled: wyrd's queries are
//! synchronous rusqlite reads, so a blocking line loop over three methods
//! doesn't warrant an async SDK dependency.

#![forbid(unsafe_code)]

use serde_json::{json, Value};
use wyrd_core::Recording;

/// The MCP protocol revision this server was written against.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Handle one incoming JSON-RPC message (a single line of the stdio
/// transport). Returns the response to write back, or `None` when nothing is
/// due (notifications, and client responses to server requests — we send
/// none, but a well-behaved peer must not be answered back).
pub fn handle_line(line: &str) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(error_response(
                Value::Null,
                -32700,
                format!("parse error: {e}"),
            ))
        }
    };
    // No method → a response to a server request, not a request/notification.
    let method = msg.get("method").and_then(Value::as_str)?;
    let id = msg.get("id").cloned();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    let id = id?; // notifications (initialized, cancelled, …) are never answered
    match method {
        "initialize" => Some(result_response(id, initialize_result(&params))),
        "ping" => Some(result_response(id, json!({}))),
        "tools/list" => Some(result_response(id, json!({ "tools": tool_definitions() }))),
        "tools/call" => Some(result_response(id, call_tool(&params))),
        other => Some(error_response(
            id,
            -32601,
            format!("method not found: {other}"),
        )),
    }
}

fn result_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize_result(params: &Value) -> Value {
    // Accept whatever revision the client asked for: every method we speak
    // exists unchanged in all published revisions.
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "wyrd-mcp",
            "title": "wyrd — async causality inspection for tokio recordings",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "Inspect .wyrd recordings of tokio applications. \
            Start with `stats` for an overview, then `why_blocked` to walk a \
            stuck task's park → resource → holder chain (it names deadlock \
            cycles). `world_state` lists every task and resource at an \
            instant; its task names are valid `task` selectors for \
            `why_blocked`. All timestamps are nanoseconds since the start of \
            the recording.",
    })
}

fn tool_definitions() -> Value {
    let recording = json!({
        "type": "string",
        "description": "Path to the .wyrd recording file",
    });
    let at = json!({
        "type": "integer",
        "description": "Timestamp (ns) to evaluate at; defaults to end-of-recording",
    });
    json!([
        {
            "name": "why_blocked",
            "description": "Explain why a task is blocked: walk its park → \
                resource → holder chain and report the outcome (deadlock \
                cycle, backpressure/timer root, or an active holder that \
                hasn't released yet). If no task is named, picks a task \
                parked behind another task.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "recording": recording,
                    "task": {
                        "type": "string",
                        "description": "Task selector: a `task::Builder` name or numeric span id",
                    },
                    "at": at,
                },
                "required": ["recording"],
            },
        },
        {
            "name": "stats",
            "description": "Summarize a recording: duration, task/resource \
                counts, poll-time percentiles, longest parks, and peak \
                channel depths. A good first call on an unfamiliar recording.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "recording": recording,
                    "top": {
                        "type": "integer",
                        "description": "How many longest-parks to include (default 10)",
                    },
                },
                "required": ["recording"],
            },
        },
        {
            "name": "world_state",
            "description": "Snapshot every task (running/idle/parked/done) \
                and resource (holder, locked, permits, depth) at an instant. \
                Use it to find task names and to see who holds what.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "recording": recording,
                    "at": at,
                },
                "required": ["recording"],
            },
        },
    ])
}

/// Run a tool and wrap the outcome as a `CallToolResult`. Tool failures (bad
/// path, unknown task, …) are reported in-band via `isError`, per spec, so
/// the model can see and react to them.
fn call_tool(params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match run_tool(name, &args) {
        Ok(value) => json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&value).unwrap_or_default(),
            }],
            "structuredContent": value,
            "isError": false,
        }),
        Err(message) => json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        }),
    }
}

fn run_tool(name: &str, args: &Value) -> Result<Value, String> {
    let path = args
        .get("recording")
        .and_then(Value::as_str)
        .ok_or("missing required argument: recording")?;
    let rec = Recording::open(path).map_err(|e| format!("cannot open {path}: {e}"))?;
    let at = args.get("at").and_then(Value::as_u64);

    let report = match name {
        "why_blocked" => {
            let task = match args.get("task").and_then(Value::as_str) {
                Some(sel) => rec.resolve_task(sel).map_err(|e| e.to_string())?,
                None => rec
                    .pick_blocked_task(at)
                    .map_err(|e| e.to_string())?
                    .ok_or("recording contains no tasks")?,
            };
            serde_json::to_value(rec.why_blocked(task, at).map_err(|e| e.to_string())?)
        }
        "stats" => {
            let top = args.get("top").and_then(Value::as_u64).unwrap_or(10) as usize;
            serde_json::to_value(rec.stats(top).map_err(|e| e.to_string())?)
        }
        "world_state" => serde_json::to_value(rec.world_state(at).map_err(|e| e.to_string())?),
        other => return Err(format!("unknown tool: {other}")),
    };
    report.map_err(|e| format!("serialize result: {e}"))
}
