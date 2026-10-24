//! The real `tunemend` binary: the same tool calls through the CLI and
//! through MCP stdio give the same results.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

const BINARY: &str = env!("CARGO_BIN_EXE_tunemend");

fn workspace(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("tunemend-bin-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn steps() -> Vec<(&'static str, Value)> {
    let bundle = common::village();
    let mut request =
        serde_json::to_value(bundle.revisions[1].edit_request.as_ref().unwrap()).unwrap();
    request.as_object_mut().unwrap().remove("feedback");
    vec![
        (
            "create_project",
            json!({"job_id": "create", "project": "village", "timebase": bundle.project.timebase, "composition": bundle.revisions[0].composition}),
        ),
        (
            "edit_project",
            json!({"project": "village", "request": request}),
        ),
        (
            "edit_project",
            json!({"project": "village", "request": request}),
        ),
        (
            "check_project",
            json!({"project": "village", "profile": "test-four-channel"}),
        ),
        (
            "render_preview",
            json!({"job_id": "render", "project": "village", "layout": {"kind": "excerpt", "start": 5760, "end": 7680}}),
        ),
        (
            "compare_revisions",
            json!({"project": "village", "from": "r1", "to": "r2"}),
        ),
        (
            "edit_project",
            json!({"project": "village", "request": {"job_id": "bad", "base_revision": "r1", "operations": []}}),
        ),
    ]
}

fn call_cli(workspace: &Path, tool: &str, input: &Value) -> (Value, i32) {
    let mut child = Command::new(BINARY)
        .args(["call", tool, "--workspace"])
        .arg(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    (
        serde_json::from_slice(&output.stdout).unwrap(),
        output.status.code().unwrap(),
    )
}

/// Paths contain the workspace; replace it so both runs compare equal.
fn normalized(mut value: Value, workspace: &Path) -> Value {
    fn walk(value: &mut Value, prefix: &str) {
        match value {
            Value::String(s) if s.contains(prefix) => *s = s.replace(prefix, "<workspace>"),
            Value::Array(items) => items.iter_mut().for_each(|v| walk(v, prefix)),
            Value::Object(map) => map.values_mut().for_each(|v| walk(v, prefix)),
            _ => {}
        }
    }
    walk(&mut value, &workspace.display().to_string());
    value
}

struct McpClient {
    child: std::process::Child,
    reader: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl McpClient {
    fn start(workspace: &Path) -> Self {
        let mut child = Command::new(BINARY)
            .args(["mcp", "--workspace"])
            .arg(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        let mut client = Self {
            child,
            reader,
            next_id: 1,
        };
        let init = client.request("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}));
        assert!(init["result"]["serverInfo"]["name"] == "tunemend", "{init}");
        assert_eq!(
            init["result"]["instructions"],
            include_str!("../src/mcp_instructions.md")
        );
        client.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        client
    }

    fn send(&mut self, message: Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            assert!(
                self.reader.read_line(&mut line).unwrap() > 0,
                "server closed"
            );
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == id {
                return message;
            }
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn cli_and_mcp_give_the_same_results() {
    let (cli_space, mcp_space) = (workspace("cli"), workspace("mcp"));
    let mut mcp = McpClient::start(&mcp_space);
    let listed = mcp.request("tools/list", json!({}));
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        tunemend::service::tool_definitions()
            .iter()
            .map(|t| t.name)
            .collect::<Vec<_>>()
    );
    assert!(names.contains(&"present_review") && names.contains(&"read_feedback"));

    for (index, (tool, input)) in steps().into_iter().enumerate() {
        let (cli, exit) = call_cli(&cli_space, tool, &input);
        let answer = mcp.request("tools/call", json!({"name": tool, "arguments": input}));
        let structured = answer["result"]["structuredContent"].clone();
        let text: Value =
            serde_json::from_str(answer["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            text, structured,
            "text-only and structured clients must agree"
        );
        assert_eq!(
            answer["result"]["isError"],
            json!(cli["status"] != "ok"),
            "step {index}"
        );
        assert_eq!(
            normalized(cli.clone(), &cli_space),
            normalized(structured, &mcp_space),
            "step {index} {tool}"
        );
        let expected_exit = match cli["status"].as_str().unwrap() {
            "ok" => 0,
            "blocked" => 2,
            _ => 1,
        };
        assert_eq!(exit, expected_exit, "step {index}");
    }
    let (_, exit) = call_cli(
        &cli_space,
        "inspect_project",
        &json!({"view": "summary", "project": "missing"}),
    );
    assert_eq!(exit, 2);
    drop(mcp);
    let _ = std::fs::remove_dir_all(cli_space);
    let _ = std::fs::remove_dir_all(mcp_space);
}

#[test]
fn mcp_discovery_and_schema_refusals_support_recovery() {
    let space = workspace("discovery");
    let mut mcp = McpClient::start(&space);
    let listed = mcp.request("tools/list", json!({}));
    for tool in listed["result"]["tools"].as_array().unwrap() {
        jsonschema::validator_for(&tool["inputSchema"])
            .unwrap_or_else(|e| panic!("{}: {e}", tool["name"]));
    }
    for (name, args, location) in [
        (
            "create_project",
            json!({"project":"probe", "timebase":{"kind":"effect"}}),
            "/job_id",
        ),
        (
            "edit_project",
            json!({"project":"probe", "request":{"base_revision":"r1", "operations":[]}}),
            "/request/job_id",
        ),
        ("get_capabilities", json!({"section":"mixer"}), ""),
    ] {
        let answer = mcp.request("tools/call", json!({"name":name, "arguments":args}));
        assert_eq!(answer["result"]["isError"], true, "{answer}");
        let d = &answer["result"]["structuredContent"]["diagnostics"][0];
        assert_eq!(d["code"], "schema_mismatch", "{answer}");
        assert_eq!(d["location"], location);
    }
    let recovered = mcp.request(
        "tools/call",
        json!({"name":"get_capabilities", "arguments":{"sections":["mixer"]}}),
    );
    assert_eq!(recovered["result"]["isError"], false);
    assert!(recovered["result"]["structuredContent"]["result"]["mixer"].is_object());
    let empty = mcp.request(
        "tools/call",
        json!({"name":"inspect_project", "arguments":{"view":"projects"}}),
    );
    assert_eq!(
        empty["result"]["structuredContent"]["result"]["projects"],
        json!([])
    );
    drop(mcp);
    let _ = std::fs::remove_dir_all(space);
}

#[test]
fn present_review_through_mcp_starts_a_listening_screen() {
    let space = workspace("screen");
    let mut mcp = McpClient::start(&space);
    for (tool, input) in steps().into_iter().take(1).chain([(
        "render_preview",
        json!({"job_id": "render", "project": "village", "layout": {"kind": "full", "loop_passes": 1}}),
    )]) {
        let answer = mcp.request("tools/call", json!({"name": tool, "arguments": input}));
        assert_eq!(answer["result"]["isError"], false, "{answer}");
    }
    let show = json!({"name": "present_review", "arguments": {"job_id": "show", "project": "village", "title": "Café village", "renders": [{"render": "render-1", "label": "First draft"}]}});
    let presented = mcp.request("tools/call", show.clone());
    let url = presented["result"]["structuredContent"]["result"]["url"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        url.starts_with("http://127.0.0.1:") && url.ends_with("/review/village/review-1"),
        "{url}"
    );

    let (address, path) = url.trim_start_matches("http://").split_once('/').unwrap();
    let get = |path: &str| -> String {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        write!(
            stream,
            "GET /{path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut body = String::new();
        std::io::Read::read_to_string(&mut stream, &mut body).unwrap();
        body
    };
    assert!(get(path).contains("Send to AI"));
    let page = get("api/review/village/review-1");
    assert!(page.contains("\"label\":\"First draft\""), "{page}");

    // A retry after restart must retain the presentation but use the live
    // listener. Occupy the old port so reuse cannot hide a stale stored URL.
    drop(mcp);
    let old_port = std::net::TcpListener::bind(address).unwrap();
    let mut resumed = McpClient::start(&space);
    let replay = resumed.request("tools/call", show);
    let result = &replay["result"]["structuredContent"];
    assert_eq!(result["replayed"], true);
    assert_eq!(
        result["result"]["presentation"],
        presented["result"]["structuredContent"]["result"]["presentation"]
    );
    let live_url = result["result"]["url"].as_str().unwrap();
    assert_ne!(live_url, url, "a stored port is not a live endpoint");
    assert!(result["summary"].as_str().unwrap().contains(live_url));
    let live_address = live_url
        .trim_start_matches("http://")
        .split_once('/')
        .unwrap()
        .0;
    let mut stream = std::net::TcpStream::connect(live_address).unwrap();
    write!(stream, "GET /api/review/village/review-1 HTTP/1.1\r\nHost: {live_address}\r\nConnection: close\r\n\r\n").unwrap();
    let mut page = String::new();
    std::io::Read::read_to_string(&mut stream, &mut page).unwrap();
    assert!(page.contains("\"label\":\"First draft\""), "{page}");
    drop(resumed);
    drop(old_port);
    let _ = std::fs::remove_dir_all(space);
}
