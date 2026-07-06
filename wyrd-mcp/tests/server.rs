//! End-to-end tests for the MCP dispatch: write a real recording file with
//! wyrd-weave, then drive `handle_line` with JSON-RPC messages exactly as the
//! stdio transport would deliver them.

use std::path::PathBuf;

use serde_json::{json, Value};
use wyrd_mcp::handle_line;
use wyrd_weave::{file_writer, Event, Loc, Record, StateOp, TaskKind, FIELD_ACQUIRED_BY};

/// Write the canonical two-mutex deadlock (t1 holds A wants B, t2 holds B
/// wants A) to a fresh recording file and return its path.
fn deadlock_recording_file(tag: &str) -> PathBuf {
    let loc = |line: u32| Loc {
        file: Some("src/main.rs".into()),
        line: Some(line),
        col: None,
    };
    let mutex = |id: u64, line: u32| Event::ResourceNew {
        id,
        parent: None,
        concrete_type: "Mutex".into(),
        loc: loc(line),
        is_internal: false,
    };
    let spawn = |id: u64, name: &str| Event::TaskSpawn {
        id,
        parent: None,
        name: Some(name.into()),
        loc: loc(1),
        kind: TaskKind::Task,
    };
    let acquired_by = |id: u64, task: u64| Event::ResourceState {
        id,
        field: FIELD_ACQUIRED_BY.into(),
        value: task as i64,
        op: StateOp::Override,
    };
    let park = |task: u64, resource: u64| Event::Park {
        task,
        resource,
        op_name: "poll_acquire".into(),
    };

    let events = vec![
        (1, mutex(100, 10)),
        (2, mutex(200, 20)),
        (3, spawn(1, "t1")),
        (4, spawn(2, "t2")),
        (5, Event::PollStart { task: 1 }),
        (6, acquired_by(100, 1)),
        (7, Event::PollEnd { task: 1 }),
        (8, Event::PollStart { task: 2 }),
        (9, acquired_by(200, 2)),
        (10, Event::PollEnd { task: 2 }),
        (11, Event::PollStart { task: 1 }),
        (12, park(1, 200)),
        (13, Event::PollEnd { task: 1 }),
        (14, Event::PollStart { task: 2 }),
        (15, park(2, 100)),
        (16, Event::PollEnd { task: 2 }),
    ];

    let path =
        std::env::temp_dir().join(format!("wyrd-mcp-test-{}-{tag}.wyrd", std::process::id()));
    let mut writer = file_writer(&path).expect("open recording for writing");
    for (ts, event) in events {
        writer
            .write_record(&Record { ts, event })
            .expect("write record");
    }
    writer.flush().expect("flush recording");
    path
}

/// Send a request and unwrap `result`, panicking on a JSON-RPC error.
fn request(msg: Value) -> Value {
    let response = handle_line(&msg.to_string()).expect("request deserves a response");
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], msg["id"]);
    assert!(
        response.get("error").is_none(),
        "unexpected error: {response}"
    );
    response["result"].clone()
}

fn call_tool(name: &str, arguments: Value) -> Value {
    request(json!({
        "jsonrpc": "2.0", "id": 9, "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    }))
}

#[test]
fn initialize_handshake() {
    let result = request(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0" },
        },
    }));
    assert_eq!(result["protocolVersion"], "2025-06-18");
    assert_eq!(result["serverInfo"]["name"], "wyrd-mcp");
    assert!(result["capabilities"]["tools"].is_object());

    // The follow-up notification gets no reply.
    let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    assert_eq!(handle_line(&note.to_string()), None);
}

#[test]
fn lists_the_three_tools() {
    let result = request(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let names: Vec<&str> = result["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("tool name"))
        .collect();
    assert_eq!(names, ["why_blocked", "stats", "world_state"]);
    for tool in result["tools"].as_array().unwrap() {
        assert_eq!(tool["inputSchema"]["required"], json!(["recording"]));
    }
}

#[test]
fn why_blocked_names_the_deadlock() {
    let path = deadlock_recording_file("why-blocked");
    // No `task` argument: the server should auto-pick a task parked behind
    // another and report the 2-cycle.
    let result = call_tool("why_blocked", json!({ "recording": &path }));
    assert_eq!(result["isError"], false);

    let report = &result["structuredContent"];
    let cycle = report["outcome"]["cycle"]
        .as_array()
        .expect("deadlock cycle");
    assert_eq!(cycle.len(), 2);
    assert_eq!(report["chain"].as_array().unwrap().len(), 2);

    // The text content carries the same report for plain-text clients.
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("deadlock"),
        "text should name the outcome: {text}"
    );

    // Selecting by task name works too.
    let by_name = call_tool("why_blocked", json!({ "recording": &path, "task": "t2" }));
    assert_eq!(by_name["structuredContent"]["task"], 2);
}

#[test]
fn stats_counts_tasks_and_resources() {
    let path = deadlock_recording_file("stats");
    let result = call_tool("stats", json!({ "recording": &path, "top": 5 }));
    assert_eq!(result["isError"], false);
    let stats = &result["structuredContent"];
    assert_eq!(stats["task_count"], 2);
    assert_eq!(stats["resource_count"], 2);
}

#[test]
fn world_state_snapshots_tasks_and_holders() {
    let path = deadlock_recording_file("world-state");
    let result = call_tool("world_state", json!({ "recording": &path }));
    let world = &result["structuredContent"];
    assert_eq!(world["tasks"].as_array().unwrap().len(), 2);
    let parked: Vec<&str> = world["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["state"].as_str().unwrap())
        .collect();
    assert_eq!(parked, ["parked", "parked"]);
}

#[test]
fn tool_errors_are_in_band() {
    let missing = call_tool("stats", json!({ "recording": "/no/such/file.wyrd" }));
    assert_eq!(missing["isError"], true);
    let text = missing["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("/no/such/file.wyrd"));

    let unknown = call_tool("frobnicate", json!({ "recording": "/nope.wyrd" }));
    assert_eq!(unknown["isError"], true);

    let no_arg = call_tool("stats", json!({}));
    assert_eq!(no_arg["isError"], true);
    assert!(no_arg["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("recording"));
}

#[test]
fn protocol_errors_and_notifications() {
    // Unknown method with an id → JSON-RPC error, not a crash.
    let response = handle_line(r#"{"jsonrpc":"2.0","id":7,"method":"resources/list"}"#)
        .expect("errors get responses");
    assert_eq!(response["error"]["code"], -32601);

    // Malformed JSON → parse error with a null id.
    let response = handle_line("{ not json").expect("parse errors get responses");
    assert_eq!(response["error"]["code"], -32700);
    assert_eq!(response["id"], Value::Null);

    // Pings keep the session alive.
    let pong = request(json!({ "jsonrpc": "2.0", "id": 8, "method": "ping" }));
    assert_eq!(pong, json!({}));
}
