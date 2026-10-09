//! Drive the real binary over MCP stdio: initialize, list tools, call brief_user,
//! submit from the "browser" using the URL from the progress notification, and check
//! the tool result. Also exercises the elicitation hold path with a fake Codex client, and a
//! briefing outliving its MCP server and the hub it started.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod common;

use common::Machine;

struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl McpClient {
    fn spawn(machine: &Machine, args: &[&str], client_name: &str, elicitation: bool) -> Self {
        let mut child = machine
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn briefing mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut client = Self { child, stdin, stdout };
        let mut capabilities = json!({});
        if elicitation {
            capabilities["elicitation"] = json!({});
        }
        let init = client.request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": capabilities,
                "clientInfo": {"name": client_name, "version": "0.0.0"}
            }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "briefing");
        assert_eq!(init["result"]["instructions"], briefing::guidance::mcp_guidance());
        client.notify("notifications/initialized", json!({}));
        client
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn send_request(&mut self, id: u64, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).unwrap();
        assert!(n > 0, "server closed stdout");
        serde_json::from_str(line.trim()).unwrap_or_else(|e| panic!("bad json {e}: {line}"))
    }

    /// Read until the response with `id` arrives; other messages go to `on_other`.
    fn read_response(&mut self, id: u64, mut on_other: impl FnMut(&mut Self, Value)) -> Value {
        loop {
            let message = self.read();
            if message.get("id") == Some(&json!(id))
                && (message.get("result").is_some() || message.get("error").is_some())
            {
                return message;
            }
            on_other(self, message);
        }
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send_request(id, method, params);
        self.read_response(id, |_, other| panic!("unexpected message {other}"))
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn demo_presentation() -> Value {
    serde_json::to_value(briefing::content::demo()).unwrap()
}

fn submit(url: &str, body: Value) {
    briefing::tls::init();
    let (origin, id) = url.rsplit_once("/briefing/").unwrap();
    let client = common::blocking_client();
    let response =
        client.post(format!("{origin}/api/{id}/complete")).header("origin", origin).json(&body).send().unwrap();
    assert_eq!(response.status(), 200);
}

#[test]
fn progress_hold_roundtrip() {
    let machine = Machine::new();
    let mut client = McpClient::spawn(&machine, &["mcp"], "claude-code", false);

    let tools = client.request(2, "tools/list", json!({}));
    let mut names: Vec<&str> =
        tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(names, ["await_briefing", "brief_user", "cancel_briefing"]);
    let present = tools["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "brief_user").unwrap();
    let schema = &present["inputSchema"];
    assert_eq!(schema["properties"]["chunks"]["maxItems"], 10);

    // Invalid input -> tool error (not a protocol error).
    let bad = client.request(
        3,
        "tools/call",
        json!({"name": "brief_user", "arguments": {"title": "x", "goal": "g", "chunks": [{"title": "t", "mainPoint": "  "}]}}),
    );
    assert!(bad["error"]["message"].as_str().unwrap().contains("mainPoint"), "{bad}");

    let opened = client.request(4, "tools/call", json!({"name": "brief_user", "arguments": demo_presentation()}));
    let open = &opened["result"]["structuredContent"];
    assert_eq!(open["status"], "active", "{opened}");
    let url = open["url"].as_str().unwrap().to_string();
    let id = open["briefingId"].as_str().unwrap().to_string();
    assert!(url.starts_with("http://127.0.0.1:"));
    assert!(opened["result"]["content"][0]["text"].as_str().unwrap().contains(&url));
    assert!(open["instructions"].as_str().unwrap().contains(&url));

    client.send_request(
        5,
        "tools/call",
        json!({"name": "await_briefing", "arguments": {"briefingId": id}, "_meta": {"progressToken": "p1"}}),
    );
    let mut submitted = false;
    let response = client.read_response(5, |_, message| {
        assert_eq!(message["method"], "notifications/progress");
        assert_eq!(message["params"]["progressToken"], "p1");
        assert!(message["params"]["message"].as_str().unwrap().contains(&url));
        if !submitted {
            submitted = true;
            submit(&url, {
                let mut body = common::demo_submission(&["looks right"]);
                body["questions"][0]["selected"] = json!(["Paced, one chunk per screen"]);
                body
            });
        }
    });
    let result = &response["result"];
    assert_eq!(result["isError"], false, "{response}");
    assert_eq!(result["structuredContent"]["status"], "completed");
    let feedback = &result["structuredContent"]["feedback"];
    assert_eq!(feedback["notes"][0], "looks right");
    assert_eq!(feedback["questions"][0]["selected"][0], "Paced, one chunk per screen");
    assert_eq!(feedback["questions"][0]["status"], "answered");

    // Unknown briefing id for await_briefing.
    let unknown =
        client.request(5, "tools/call", json!({"name": "await_briefing", "arguments": {"briefingId": "nope"}}));
    assert!(unknown["error"]["message"].as_str().unwrap().contains("unknown briefingId"));
}

#[test]
fn pending_then_await_and_cancel() {
    let machine = Machine::new();
    let mut client = McpClient::spawn(&machine, &["mcp", "--max-wait-secs", "1"], "mcp-inspector", false);
    let opened = client.request(2, "tools/call", json!({"name": "brief_user", "arguments": demo_presentation()}));
    let id = opened["result"]["structuredContent"]["briefingId"].as_str().unwrap().to_string();
    let response = client.request(6, "tools/call", json!({"name": "await_briefing", "arguments": {"briefingId": id}}));
    assert_eq!(response["result"]["structuredContent"]["status"], "pending", "{response}");
    assert!(response["result"]["structuredContent"]["instructions"].as_str().unwrap().contains(&id));

    let again = client.request(
        3,
        "tools/call",
        json!({"name": "await_briefing", "arguments": {"briefingId": id, "waitSeconds": 1}}),
    );
    assert_eq!(again["result"]["structuredContent"]["status"], "pending");

    let cancelled =
        client.request(4, "tools/call", json!({"name": "cancel_briefing", "arguments": {"briefingId": id}}));
    assert_eq!(cancelled["result"]["structuredContent"]["cancelled"], true);
    let after = client.request(5, "tools/call", json!({"name": "await_briefing", "arguments": {"briefingId": id}}));
    assert_eq!(after["result"]["structuredContent"]["status"], "cancelled");
    assert_eq!(after["result"]["isError"], false);
}

#[test]
fn elicitation_hold_for_codex() {
    let machine = Machine::new();
    let mut client = McpClient::spawn(&machine, &["mcp"], "codex-mcp-client", true);
    let opened = client.request(10, "tools/call", json!({"name": "brief_user", "arguments": demo_presentation()}));
    let id = opened["result"]["structuredContent"]["briefingId"].as_str().unwrap().to_string();
    let status = machine.command().args(["status", &id, "--json"]).output().unwrap();
    let info: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(info["source"], format!("codex-mcp-client@{}", briefing::backend::hostname()));
    client.send_request(2, "tools/call", json!({"name": "await_briefing", "arguments": {"briefingId": id}}));

    // Server should ask us (the client) for an elicitation; answer it after submitting.
    let started = Instant::now();
    let mut elicitation_id: Option<Value> = None;
    let mut url: Option<String> = None;
    let response = client.read_response(2, |this, message| {
        if message["method"] == "elicitation/create" {
            let text = message["params"]["message"].as_str().unwrap().to_string();
            url = Some(text.split_whitespace().find(|w| w.starts_with("http://127.0.0.1:")).unwrap().to_string());
            assert_eq!(message["params"]["mode"], "form");
            elicitation_id = Some(message["id"].clone());
            submit(url.as_ref().unwrap(), common::demo_submission(&["via codex"]));
            // The server cancels the elicitation once the submission lands; respond anyway
            // to make sure a late answer is tolerated.
            this.send(json!({"jsonrpc": "2.0", "id": message["id"], "result": {"action": "accept", "content": {"submitted": true}}}));
        } else {
            assert_eq!(message["method"], "notifications/cancelled", "unexpected {message}");
            assert_eq!(message["params"]["requestId"], *elicitation_id.as_ref().unwrap());
        }
    });
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(response["result"]["structuredContent"]["status"], "completed", "{response}");
    assert_eq!(response["result"]["structuredContent"]["feedback"]["notes"][0], "via codex");

    // Declining the elicitation cancels the briefing.
    let opened = client.request(11, "tools/call", json!({"name": "brief_user", "arguments": demo_presentation()}));
    let id = opened["result"]["structuredContent"]["briefingId"].as_str().unwrap().to_string();
    client.send_request(3, "tools/call", json!({"name": "await_briefing", "arguments": {"briefingId": id}}));
    let response = client.read_response(3, |this, message| {
        if message["method"] == "elicitation/create" {
            this.send(json!({"jsonrpc": "2.0", "id": message["id"], "result": {"action": "decline"}}));
        }
    });
    assert_eq!(response["result"]["structuredContent"]["status"], "cancelled", "{response}");
}

/// A briefing outlives the MCP server that opened it and the hub that server started: a fresh
/// server's await_briefing starts the hub again, which serves the same link with the briefing
/// intact, and the feedback comes back through it.
#[test]
fn briefing_outlives_its_server_and_hub() {
    let machine = Machine::with_fixed_port();
    let mut first = McpClient::spawn(&machine, &["mcp"], "mcp-inspector", false);
    let opened = first.request(2, "tools/call", json!({"name": "brief_user", "arguments": demo_presentation()}));
    let id = opened["result"]["structuredContent"]["briefingId"].as_str().unwrap().to_string();
    let url = opened["result"]["structuredContent"]["url"].as_str().unwrap().to_string();
    assert!(opened["result"]["structuredContent"]["instructions"].as_str().unwrap().contains(&url));
    drop(first);
    machine.stop_hub();
    // Wait for the old hub to let go of the port.
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::net::TcpStream::connect(("127.0.0.1", machine.port)).is_ok() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut second = McpClient::spawn(&machine, &["mcp", "--max-wait-secs", "30"], "mcp-inspector", false);
    second.send_request(3, "tools/call", json!({"name": "await_briefing", "arguments": {"briefingId": id}}));
    // The await restarts the hub; submit through the original link once it answers.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !reqwest::blocking::get(&url).is_ok_and(|r| r.status() == 200) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    submit(&url, common::demo_submission(&["after restart"]));
    let done = second.read_response(3, |_, _| {});
    assert_eq!(done["result"]["structuredContent"]["status"], "completed", "{done}");
    assert_eq!(done["result"]["structuredContent"]["feedback"]["notes"][0], "after restart");

    // Another server gets the stored result straight away.
    let mut third = McpClient::spawn(&machine, &["mcp"], "mcp-inspector", false);
    let stored = third.request(4, "tools/call", json!({"name": "await_briefing", "arguments": {"briefingId": id}}));
    assert_eq!(stored["result"]["structuredContent"]["status"], "completed");
    assert_eq!(stored["result"]["structuredContent"]["feedback"]["notes"][0], "after restart");
}

/// A wait already in progress survives its hub going away mid-long-poll (exit, idle exit, or
/// replacement by a newer client): the waiting server starts the hub again on the same port,
/// and the submission made through the original link reaches it.
#[test]
fn a_waiting_await_survives_its_hub_restarting() {
    let machine = Machine::with_fixed_port();
    let mut client = McpClient::spawn(&machine, &["mcp", "--max-wait-secs", "60"], "mcp-inspector", false);
    let opened = client.request(2, "tools/call", json!({"name": "brief_user", "arguments": demo_presentation()}));
    let id = opened["result"]["structuredContent"]["briefingId"].as_str().unwrap().to_string();
    let url = opened["result"]["structuredContent"]["url"].as_str().unwrap().to_string();
    client.send_request(3, "tools/call", json!({"name": "await_briefing", "arguments": {"briefingId": id}}));
    // Let the long-poll reach the hub before pulling it out from under the wait.
    std::thread::sleep(Duration::from_millis(500));
    let killed = briefing::local_hub::HubFile::read(machine.state.path()).unwrap().pid;
    machine.stop_hub();

    // Nothing else is a client of this machine: only the waiting server can bring the hub back.
    let deadline = Instant::now() + Duration::from_secs(15);
    let restarted = loop {
        let pid = briefing::local_hub::HubFile::read(machine.state.path()).map(|file| file.pid);
        if pid.is_some_and(|pid| pid != killed) && reqwest::blocking::get(&url).is_ok_and(|r| r.status() == 200) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(restarted, "the waiting server did not restart the hub");
    submit(&url, common::demo_submission(&["survived"]));
    let done = client.read_response(3, |_, _| {});
    assert_eq!(done["result"]["structuredContent"]["status"], "completed", "{done}");
    assert_eq!(done["result"]["structuredContent"]["feedback"]["notes"][0], "survived");
}
