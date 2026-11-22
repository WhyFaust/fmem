#![cfg(feature = "local-embeddings")]

use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Output, Stdio},
    sync::mpsc::{self, Receiver},
    time::Duration,
};
use tempfile::TempDir;

struct Fixture(TempDir);
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("vault/.obsidian")).unwrap();
        fs::create_dir(temp.path().join("project")).unwrap();
        let result = Self(temp);
        result.ok(
            &[
                "init",
                "--vault",
                result.0.path().join("vault").to_str().unwrap(),
                "--project",
                "example",
            ],
            None,
        );
        result
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fmem"));
        command
            .arg("--state-dir")
            .arg(self.0.path().join("state"))
            .arg("--project-dir")
            .arg(self.0.path().join("project"));
        command
    }
    fn run(&self, args: &[&str], input: Option<&Value>) -> Output {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(input) = input {
            write!(child.stdin.take().unwrap(), "{input}").unwrap();
        }
        child.wait_with_output().unwrap()
    }
    fn ok(&self, args: &[&str], input: Option<&Value>) -> Value {
        let output = self.run(args, input);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn batch() -> Value {
    json!({"operations":[
        {"operation_id":"case.preference", "action":"remember", "candidate":{
            "kind":"preference", "title":"Язык ответов", "body":"Отвечать по-русски и приводить проверяемые примеры."}},
        {"operation_id":"case.decision", "action":"remember", "candidate":{
            "kind":"decision", "title":"Хранилище результатов", "body":"Результаты тестирования сохраняем в SQLite."}}
    ]})
}

#[test]
fn cli_retries_and_context_keep_preferences_and_sources() {
    let fixture = Fixture::new();
    let first = fixture.ok(&["commit"], Some(&batch()));
    let replay = fixture.ok(&["commit"], Some(&batch()));
    assert_eq!(first["receipts"], replay["receipts"]);
    assert!(first["receipts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["status"] == "persisted"));
    let context = fixture.ok(
        &[
            "context",
            "результаты тестирования",
            "--mode",
            "lexical",
            "--max-bytes",
            "2000",
        ],
        None,
    );
    assert!(!context["preferences"].as_array().unwrap().is_empty());
    assert!(!context["matches"].as_array().unwrap().is_empty());
    assert!(context.to_string().len() <= 2000);
    let hit = &context["matches"][0];
    assert!(hit["source"].as_str().is_some());
    let record = fixture.ok(&["read", hit["id"].as_str().unwrap()], None);
    assert!(record["body"].as_str().unwrap().contains("SQLite"));
    let denied = fixture.run(&["--read-only", "commit"], Some(&batch()));
    assert!(!denied.status.success());
}

struct Client {
    process: Child,
    messages: Receiver<Value>,
}
impl Client {
    fn new(fixture: &Fixture, read_only: bool) -> Self {
        let mut command = fixture.command();
        if read_only {
            command.arg("--read-only");
        }
        let mut process = command
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = process.stdout.take().unwrap();
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let value =
                    serde_json::from_str(&line).unwrap_or_else(|_| json!({"invalid_stdout": line}));
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        let mut client = Self { process, messages };
        let init = client.request(1, "initialize", json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fmem-test","version":"1"}}));
        assert!(
            init["result"]["capabilities"]["tools"].is_object(),
            "{init}"
        );
        client.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        client
    }
    fn send(&mut self, value: Value) {
        let stdin = self.process.stdin.as_mut().unwrap();
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    }
    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        loop {
            let value = self
                .messages
                .recv_timeout(Duration::from_secs(15))
                .expect("MCP response deadline");
            assert!(value.get("invalid_stdout").is_none(), "{value}");
            if value["id"] == id {
                return value;
            }
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.process.stdin.take();
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

#[test]
fn mcp_round_trip_and_read_only_boundary() {
    let fixture = Fixture::new();
    let mut client = Client::new(&fixture, false);
    let write = client.request(
        2,
        "tools/call",
        json!({"name":"memory_commit","arguments":batch()}),
    );
    assert_eq!(
        write["result"]["structuredContent"]["receipts"][0]["status"], "persisted",
        "{write}"
    );
    let context = client.request(
        3,
        "tools/call",
        json!({"name":"memory_context","arguments":{"query":"результаты","mode":"lexical"}}),
    );
    assert!(
        !context["result"]["structuredContent"]["matches"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{context}"
    );
    drop(client);
    let mut client = Client::new(&fixture, true);
    let available = client.request(4, "tools/list", json!({}));
    assert_eq!(available["result"]["tools"].as_array().unwrap().len(), 3);
    let denied = client.request(
        5,
        "tools/call",
        json!({"name":"memory_commit","arguments":batch()}),
    );
    assert!(denied.get("error").is_some(), "{denied}");
}

#[test]
fn live_mcp_rejects_rebinding_instead_of_switching_project() {
    let fixture = Fixture::new();
    let mut client = Client::new(&fixture, false);
    fixture.ok(
        &[
            "init",
            "--vault",
            fixture.0.path().join("vault").to_str().unwrap(),
            "--project",
            "other",
        ],
        None,
    );
    let write = client.request(
        2,
        "tools/call",
        json!({"name":"memory_commit","arguments":batch()}),
    );
    assert_eq!(write["result"]["isError"], true, "{write}");
    assert!(write["result"]["structuredContent"]["error"]
        .as_str()
        .unwrap()
        .contains("binding changed"));
    let search = fixture.ok(&["search", "SQLite", "--mode", "lexical"], None);
    assert_eq!(search["hits"], json!([]));
}

#[test]
fn cli_accepts_and_reports_operations_while_vault_is_offline() {
    let fixture = Fixture::new();
    let vault = fixture.0.path().join("vault");
    let offline = fixture.0.path().join("offline");
    fs::rename(&vault, &offline).unwrap();
    let accepted = fixture.ok(&["commit"], Some(&batch()));
    assert!(accepted["receipts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["status"] == "retryable"));
    let receipt = fixture.ok(&["receipt", "case.preference"], None);
    assert_eq!(receipt["receipt"]["status"], "retryable");
    assert_eq!(fixture.ok(&["status"], None)["vault_available"], false);
    fs::rename(&offline, &vault).unwrap();
    let recovered = fixture.ok(&["retry"], None);
    assert!(recovered["receipts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["status"] == "persisted"));
}
