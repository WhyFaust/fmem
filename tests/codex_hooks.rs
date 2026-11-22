#![cfg(feature = "local-embeddings")]

use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

struct Fixture {
    temp: tempfile::TempDir,
    cwd: PathBuf,
    vault: PathBuf,
    state: PathBuf,
    profile: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let base = temp.path();
        let cwd = base.join("workspace");
        let vault = base.join("vault");
        let profile = base.join("codex");
        let state = base.join("state");
        fs::create_dir(&cwd).unwrap();
        fs::create_dir_all(vault.join(".obsidian")).unwrap();
        fs::create_dir_all(&profile).unwrap();
        let fixture = Self {
            temp,
            cwd,
            vault,
            state,
            profile,
        };
        fixture.call(
            &[
                "init",
                "--vault",
                fixture.vault.to_str().unwrap(),
                "--project",
                "pilot",
            ],
            None,
        );
        fixture
    }

    fn run(&self, args: &[&str], input: Option<&Value>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fmem"));
        command
            .current_dir(&self.cwd)
            .env("CODEX_HOME", &self.profile)
            .env("LOCALAPPDATA", self.temp.path().join("cache"))
            .env("XDG_CACHE_HOME", self.temp.path().join("cache"))
            .env(
                "XDG_STATE_HOME",
                self.temp.path().join("fresh-default-state"),
            )
            .env_remove("FMEM_STATE_DIR");
        command.arg("--state-dir").arg(&self.state);
        let mut child = command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(input) = input {
            writeln!(child.stdin.take().unwrap(), "{input}").unwrap();
        }
        child.wait_with_output().unwrap()
    }

    fn call(&self, args: &[&str], input: Option<&Value>) -> Value {
        let result = self.run(args, input);
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        serde_json::from_slice(&result.stdout).unwrap()
    }

    fn hook(&self, event: &str, session: &str, turn: &str, extra: Value) -> Value {
        let mut input = json!({
            "hook_event_name":event, "session_id":session, "turn_id":turn,
            "cwd":self.cwd, "permission_mode":"default",
        });
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let output = self.call(&["codex", "hook", event], Some(&input));
        assert!(output.get("systemMessage").is_none(), "{output}");
        output
    }

    fn start(&self, turn: &str, prompt: &str) -> Value {
        self.hook(
            "UserPromptSubmit",
            "session",
            turn,
            json!({"mode":"default","prompt":prompt}),
        )
    }

    fn stop(&self, turn: &str, active: bool) -> Value {
        self.hook(
            "Stop",
            "session",
            turn,
            json!({
                "mode":"default", "last_assistant_message":"The requested work is complete.",
                "stop_hook_active":active
            }),
        )
    }

    fn stage(&self, token: &str, request: &Value) -> Value {
        self.call(&["codex", "stage", "--turn", token], Some(request))
    }

    fn read(&self, query: &str) -> Value {
        self.call(&["search", query, "--mode", "lexical"], None)["hits"].clone()
    }

    fn transcript(&self, session: &str, header: Value) -> PathBuf {
        let folder = self.profile.join("sessions/2026/01/01");
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join(format!("rollout-2026-01-01T00-00-00-{session}.jsonl"));
        fs::write(
            &path,
            format!("{}\n", json!({"type":"session_meta","payload":header})),
        )
        .unwrap();
        path
    }

    fn timeline(&self, path: &Path, turn: &str, mode: &str, parent: Option<&str>) {
        let mut start =
            json!({"type":"task_started","turn_id":turn,"collaboration_mode_kind":mode});
        if let Some(parent) = parent {
            start["root_turn_id"] = json!(parent);
            start["turn_attribution"] = json!({"turn_id":turn,"turn_trigger":"app_tool_create_thread",
                "parent_turn_id":parent, "root_turn_id":parent});
        }
        self.append_turn(path, turn, mode, start);
    }

    fn native_timeline(&self, path: &Path, turn: &str, mode: &str, parent: &str, root: &str) {
        self.append_turn(path, turn, mode, json!({
            "type":"task_started","turn_id":turn,"collaboration_mode_kind":mode,"root_turn_id":root,
            "turn_attribution":{"turn_id":turn,"turn_trigger":null,"parent_turn_id":parent,"root_turn_id":root}
        }));
    }

    fn append_turn(&self, path: &Path, turn: &str, mode: &str, start: Value) {
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{}", json!({"type":"event_msg","payload":start})).unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"turn_context","payload":{"turn_id":turn,"cwd":self.cwd,
            "collaboration_mode":{"mode":mode}}})
        )
        .unwrap();
    }
}

fn context(output: &Value) -> &str {
    output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("")
}

fn token(text: &str) -> &str {
    text.split_once("codex stage --turn ")
        .unwrap()
        .1
        .split_whitespace()
        .next()
        .unwrap()
}

fn batch(title: &str) -> Value {
    json!({"operations":[{"action":"remember","candidate":{
        "kind":"decision","title":title,"body":"Use the committed verification fixture.","source":"Repository verification output"
    }}]})
}

#[test]
fn plugin_registers_a_separate_quiet_asynchronous_worker() {
    let f = Fixture::new();
    let hooks = f.call(&["codex", "hooks"], None);
    let prompt = hooks["hooks"]["UserPromptSubmit"][0]["hooks"]
        .as_array()
        .unwrap();
    assert_eq!(prompt.len(), 2);
    assert_eq!(prompt[0]["timeout"], 5);
    assert_ne!(prompt[0]["async"], true);
    assert_eq!(prompt[1]["async"], true);
    assert_eq!(prompt[1]["timeout"], 600);
    for command in ["command", "commandWindows"] {
        assert!(prompt[0][command]
            .as_str()
            .unwrap()
            .contains("UserPromptSubmit"));
        assert!(prompt[1][command].as_str().unwrap().contains("WorkerHook"));
    }
    assert!(prompt[1].get("statusMessage").is_none());

    let session = "00000000-0000-7000-8000-000000000071";
    let turn = "00000000-0000-7000-8000-000000000072";
    let path = f.transcript(session, json!({"id":session,"source":"vscode"}));
    f.timeline(&path, turn, "default", None);
    let payload = json!({
        "hook_event_name":"UserPromptSubmit","cwd":f.cwd,"session_id":session,
        "turn_id":turn,"transcript_path":path,"permission_mode":"default"
    });
    for args in [
        &["codex", "worker-hook"][..],
        &["--read-only", "codex", "worker-hook"][..],
    ] {
        let result = f.run(args, Some(&payload));
        assert!(result.status.success());
        assert!(result.stderr.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&result.stdout).unwrap(),
            json!({})
        );
    }
    assert!(!f.state.join("semantic-worker").exists());
    assert!(!f.state.join("models").exists());
}

#[test]
fn ordinary_turn_stages_once_recalls_then_skips_quietly() {
    let f = Fixture::new();
    let start = f.start("first", "Verify the compiled fixture");
    let receipt = f.stage(token(context(&start)), &batch("Zircon verification"));
    assert_eq!(receipt, json!({"staged":true}));
    assert_eq!(
        f.stage(token(context(&start)), &batch("Zircon verification")),
        receipt
    );
    assert_eq!(f.read("zircon"), json!([]));
    assert_eq!(f.stop("first", false), json!({}));
    assert_eq!(f.stop("first", false), json!({}));
    assert_eq!(f.read("zircon").as_array().unwrap().len(), 1);

    let second = f.start("second", "zircon verification");
    assert!(context(&second).contains("Zircon verification"));
    assert!(context(&second).len() < 4096);
    f.stage(token(context(&second)), &json!({"action":"skip"}));
    assert_eq!(f.stop("second", false), json!({}));
    let state = f.call(&["codex", "diagnostics"], None);
    assert_eq!(state["phase"], "skipped");
    assert_eq!(state["staged"], 0);
}

#[test]
fn stop_recovery_is_once_and_stage_requires_source() {
    let f = Fixture::new();
    let start = f.start("omitted", "Review the result");
    let input = batch("No evidence");
    let mut invalid = input.clone();
    invalid["operations"][0]["candidate"]
        .as_object_mut()
        .unwrap()
        .remove("source");
    assert!(!f
        .run(
            &["codex", "stage", "--turn", token(context(&start))],
            Some(&invalid)
        )
        .status
        .success());
    let retry = f.stop("omitted", false);
    assert_eq!(retry["decision"], "block");
    assert_eq!(
        token(retry["reason"].as_str().unwrap()),
        token(context(&start))
    );
    assert_eq!(f.stop("omitted", false), json!({}));
    assert_eq!(f.stop("omitted", true), json!({}));
    assert_eq!(f.stop("omitted", false), json!({}));
    assert_eq!(f.read("evidence"), json!([]));
}

#[test]
fn interrupt_plan_and_wrong_cwd_cannot_commit_staged_content() {
    let f = Fixture::new();
    let input = batch("Unauthorized neon");
    let start = f.start("interrupted", "Check neon");
    let grant = token(context(&start));
    f.stage(grant, &input);
    assert_eq!(
        f.hook("Interrupt", "session", "interrupted", json!({})),
        json!({})
    );
    assert!(!f
        .run(&["codex", "stage", "--turn", grant], Some(&input))
        .status
        .success());
    assert_eq!(f.stop("interrupted", false), json!({}));

    let start = f.start("planned", "Check neon");
    f.stage(token(context(&start)), &input);
    assert_eq!(
        f.hook(
            "Stop",
            "session",
            "planned",
            json!({"mode":"plan","stop_hook_active":false})
        ),
        json!({})
    );
    assert_eq!(f.stop("planned", false), json!({}));
    let bad_cwd = f.hook(
        "UserPromptSubmit",
        "session",
        "other",
        json!({
            "mode":"default","prompt":"Check neon", "cwd":f.vault,
        }),
    );
    assert_eq!(bad_cwd, json!({}));
    assert_eq!(f.read("neon"), json!([]));
}

#[test]
fn delegated_desktop_uses_early_capture_and_restricts_scope() {
    let f = Fixture::new();
    let session = "00000000-0000-7000-8000-000000000001";
    let turn = "00000000-0000-7000-8000-000000000002";
    let parent = "00000000-0000-7000-8000-000000000003";
    let path = f.transcript(
        session,
        json!({"id":session,"originator":"Codex Desktop","source":"vscode"}),
    );
    f.timeline(&path, turn, "default", Some(parent));
    let output = f.hook(
        "PostToolUse",
        session,
        turn,
        json!({"transcript_path":path}),
    );
    let grant = token(context(&output));
    let mut shared = batch("All projects");
    shared["operations"][0]["candidate"]["kind"] = json!("preference");
    shared["operations"][0]["scope"] = json!("shared");
    assert!(!f
        .run(&["codex", "stage", "--turn", grant], Some(&shared))
        .status
        .success());
    f.stage(grant, &batch("Delegated tangerine"));
    let stop = json!({"stop_hook_active":false,"transcript_path":path, "last_assistant_message":"Verified."});
    assert_eq!(f.hook("Stop", session, turn, stop.clone()), json!({}));
    assert_eq!(f.hook("Stop", session, turn, stop), json!({}));
    let found = f.read("tangerine");
    let record = f.call(&["read", found[0]["id"].as_str().unwrap()], None);
    assert_eq!(record["session_id"], session);
    assert_eq!(record["turn_id"], turn);
}

#[test]
fn child_inherits_parent_plan_and_has_one_authorized_recovery() {
    let f = Fixture::new();
    let parent = "00000000-0000-7000-8000-000000000011";
    let child = "00000000-0000-7000-8000-000000000012";
    let parent_plan = "00000000-0000-7000-8000-000000000013";
    let parent_work = "00000000-0000-7000-8000-000000000014";
    let child_plan = "00000000-0000-7000-8000-000000000015";
    let child_work = "00000000-0000-7000-8000-000000000016";
    let parent_path = f.transcript(parent, json!({"id":parent,"source":"vscode"}));
    let child_path = f.transcript(child, json!({"id":child,"source":{
        "subagent":{"thread_spawn":{"parent_thread_id":parent,"depth":1,"agent_path":"/root/worker"}}
    }}));
    f.timeline(&parent_path, parent_plan, "plan", None);
    f.native_timeline(&child_path, child_plan, "default", parent_plan, parent_plan);
    let prompt = json!({"transcript_path":child_path,"agent_id":child,"agent_type":"worker"});
    assert!(context(&f.hook("SubagentStart", parent, child_plan, prompt.clone())).is_empty());
    assert!(context(&f.hook("UserPromptSubmit", parent, child_plan, prompt.clone())).is_empty());
    let stop = |active| {
        json!({"stop_hook_active":active,"last_assistant_message":"Checked result.",
            "transcript_path":parent_path, "agent_transcript_path":child_path,"agent_id":child,"agent_type":"worker"
        })
    };
    assert_eq!(
        f.hook("SubagentStop", parent, child_plan, stop(false)),
        json!({})
    );
    f.timeline(&parent_path, parent_work, "default", None);
    f.native_timeline(&child_path, child_work, "default", parent_work, parent_work);
    let response = f.hook("SubagentStop", parent, child_work, stop(false));
    assert_eq!(response["decision"], "block");
    assert_eq!(
        f.hook("SubagentStop", parent, child_work, stop(false)),
        json!({})
    );
    f.stage(
        token(response["reason"].as_str().unwrap()),
        &batch("Subagent lavender"),
    );
    assert_eq!(
        f.hook("SubagentStop", parent, child_work, stop(true)),
        json!({})
    );
    assert_eq!(f.read("lavender").as_array().unwrap().len(), 1);
}

#[test]
fn child_prompt_and_stop_link_parent_transcript_to_child_provenance() {
    let f = Fixture::new();
    let parent = "00000000-0000-7000-8000-000000000051";
    let parent_turn = "00000000-0000-7000-8000-000000000052";
    let child = "00000000-0000-7000-8000-000000000053";
    let child_turn = "00000000-0000-7000-8000-000000000054";
    let other = "00000000-0000-7000-8000-000000000055";
    let parent_path = f.transcript(parent, json!({"id":parent,"source":"vscode"}));
    let child_path = f.transcript(
        child,
        json!({"id":child,"source":{
            "subagent":{"thread_spawn":{"parent_thread_id":parent,"depth":1}}
        }}),
    );
    f.timeline(&parent_path, parent_turn, "default", None);
    f.native_timeline(&child_path, child_turn, "default", parent_turn, parent_turn);
    let root = f.hook(
        "UserPromptSubmit",
        parent,
        parent_turn,
        json!({"transcript_path":parent_path}),
    );
    let prompt = json!({"transcript_path":child_path,"agent_id":child,"agent_type":"worker"});
    assert_eq!(
        f.hook("UserPromptSubmit", other, child_turn, prompt.clone()),
        json!({})
    );
    let start = f.hook("UserPromptSubmit", parent, child_turn, prompt);
    let grant = token(context(&start));
    assert_ne!(token(context(&root)), grant);
    let mut shared = batch("Cross-project child rule");
    shared["operations"][0]["candidate"]["kind"] = json!("preference");
    shared["operations"][0]["scope"] = json!("shared");
    assert!(!f
        .run(&["codex", "stage", "--turn", grant], Some(&shared))
        .status
        .success());
    assert_eq!(
        f.stage(grant, &batch("Subagent tourmaline")),
        json!({"staged":true})
    );
    let stop = json!({
        "agent_id":child,"agent_type":"worker","transcript_path":parent_path,
        "agent_transcript_path":child_path,"stop_hook_active":false,
        "last_assistant_message":"The fixture was verified."
    });
    assert_eq!(
        f.hook("SubagentStop", other, child_turn, stop.clone()),
        json!({})
    );
    assert_eq!(
        f.hook("SubagentStop", child, child_turn, stop.clone()),
        json!({})
    );
    assert_eq!(f.read("tourmaline"), json!([]));
    assert_eq!(
        f.hook("SubagentStop", parent, child_turn, stop.clone()),
        json!({})
    );
    assert_eq!(f.hook("SubagentStop", parent, child_turn, stop), json!({}));
    let hits = f.read("tourmaline");
    assert_eq!(hits.as_array().unwrap().len(), 1);
    let record = f.call(&["read", hits[0]["id"].as_str().unwrap()], None);
    assert_eq!(record["session_id"], child);
    assert_eq!(record["turn_id"], child_turn);
}

#[test]
fn nested_child_keeps_root_session_and_uses_the_immediate_parent_rollout() {
    let f = Fixture::new();
    let root = "00000000-0000-7000-8000-000000000081";
    let root_turn = "00000000-0000-7000-8000-000000000082";
    let parent = "00000000-0000-7000-8000-000000000083";
    let parent_turn = "00000000-0000-7000-8000-000000000084";
    let child = "00000000-0000-7000-8000-000000000085";
    let child_turn = "00000000-0000-7000-8000-000000000086";
    let root_path = f.transcript(root, json!({"id":root,"source":"vscode"}));
    let parent_path = f.transcript(
        parent,
        json!({"id":parent,"source":{
            "subagent":{"thread_spawn":{"parent_thread_id":root,"depth":1}}
        }}),
    );
    let child_path = f.transcript(
        child,
        json!({"id":child,"source":{
            "subagent":{"thread_spawn":{"parent_thread_id":parent,"depth":2}}
        }}),
    );
    f.timeline(&root_path, root_turn, "default", None);
    f.native_timeline(&parent_path, parent_turn, "default", root_turn, root_turn);
    f.native_timeline(&child_path, child_turn, "default", parent_turn, root_turn);
    f.hook(
        "UserPromptSubmit",
        root,
        root_turn,
        json!({"transcript_path":root_path}),
    );
    let prompt = json!({"transcript_path":child_path,"agent_id":child,"agent_type":"worker"});
    assert!(context(&f.hook("UserPromptSubmit", root, child_turn, prompt.clone())).is_empty());
    f.hook(
        "UserPromptSubmit",
        root,
        parent_turn,
        json!({"transcript_path":parent_path,"agent_id":parent,"agent_type":"worker"}),
    );
    let start = f.hook("UserPromptSubmit", root, child_turn, prompt);
    f.stage(token(context(&start)), &batch("Nested apophyllite"));
    let mut stop = json!({
        "agent_id":child,"agent_type":"worker","transcript_path":root_path,
        "agent_transcript_path":child_path,"stop_hook_active":false,
        "last_assistant_message":"The nested fixture was verified."
    });
    assert_eq!(
        f.hook("SubagentStop", root, child_turn, stop.clone()),
        json!({})
    );
    assert_eq!(f.read("apophyllite"), json!([]));
    stop["transcript_path"] = json!(parent_path);
    assert_eq!(f.hook("SubagentStop", root, child_turn, stop), json!({}));
    let hits = f.read("apophyllite");
    let record = f.call(&["read", hits[0]["id"].as_str().unwrap()], None);
    assert_eq!(record["session_id"], child);
    assert_eq!(record["turn_id"], child_turn);
}

#[test]
fn nested_interrupt_closes_only_its_capture() {
    for (aborted, cancel_ancestors) in [(false, false), (true, false), (true, true)] {
        let f = Fixture::new();
        let root = "00000000-0000-7000-8000-000000000101";
        let root_turn = "00000000-0000-7000-8000-000000000102";
        let parent = "00000000-0000-7000-8000-000000000103";
        let parent_turn = "00000000-0000-7000-8000-000000000104";
        let child = "00000000-0000-7000-8000-000000000105";
        let child_turn = "00000000-0000-7000-8000-000000000106";
        let sibling = "00000000-0000-7000-8000-000000000107";
        let sibling_turn = "00000000-0000-7000-8000-000000000108";
        let other = "00000000-0000-7000-8000-000000000109";
        let other_turn = "00000000-0000-7000-8000-000000000110";
        let root_path = f.transcript(root, json!({"id":root,"source":"vscode"}));
        let parent_path = f.transcript(
            parent,
            json!({"id":parent,"source":{
                "subagent":{"thread_spawn":{"parent_thread_id":root,"depth":1}}
            }}),
        );
        let child_path = f.transcript(
            child,
            json!({"id":child,"source":{
                "subagent":{"thread_spawn":{"parent_thread_id":parent,"depth":2}}
            }}),
        );
        let sibling_path = f.transcript(
            sibling,
            json!({"id":sibling,"source":{
                "subagent":{"thread_spawn":{"parent_thread_id":parent,"depth":2}}
            }}),
        );
        let other_path = f.transcript(other, json!({"id":other,"source":"vscode"}));
        for (session, turn, path) in [
            (root, root_turn, &root_path),
            (other, other_turn, &other_path),
        ] {
            f.timeline(path, turn, "default", None);
            f.hook(
                "UserPromptSubmit",
                session,
                turn,
                json!({"transcript_path":path}),
            );
        }
        for (actor, turn, path, ancestor_turn) in [
            (parent, parent_turn, &parent_path, root_turn),
            (child, child_turn, &child_path, parent_turn),
            (sibling, sibling_turn, &sibling_path, parent_turn),
        ] {
            f.native_timeline(path, turn, "default", ancestor_turn, root_turn);
            f.hook(
                "UserPromptSubmit",
                root,
                turn,
                json!({"transcript_path":path,"agent_id":actor,"agent_type":"worker"}),
            );
        }
        let phases = || {
            let database = rusqlite::Connection::open_with_flags(
                f.state.join("codex-turns.sqlite3"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let mut statement = database
                .prepare("SELECT session,turn_id,phase FROM turns ORDER BY session,turn_id")
                .unwrap();
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .unwrap();
            rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        let mut expected: Vec<_> = [
            (root, root_turn),
            (parent, parent_turn),
            (child, child_turn),
            (sibling, sibling_turn),
            (other, other_turn),
        ]
        .into_iter()
        .map(|(session, turn)| (session.to_owned(), turn.to_owned(), "capture".to_owned()))
        .collect();
        assert_eq!(phases(), expected);
        if aborted {
            let mut rollout = fs::OpenOptions::new()
                .append(true)
                .open(&child_path)
                .unwrap();
            writeln!(
                rollout,
                "{}",
                json!({"type":"event_msg","payload":{"type":"turn_aborted","turn_id":child_turn}})
            )
            .unwrap();
        }
        if cancel_ancestors {
            f.hook("Interrupt", root, root_turn, json!({}));
            f.hook(
                "Interrupt",
                root,
                parent_turn,
                json!({"transcript_path":parent_path,"agent_id":parent,"agent_type":"worker"}),
            );
            expected[0].2 = "closed".to_owned();
            expected[1].2 = "closed".to_owned();
            assert_eq!(phases(), expected);
        }
        let interrupt =
            json!({"transcript_path":child_path,"agent_id":child,"agent_type":"worker"});
        assert_eq!(
            f.hook("Interrupt", other, child_turn, interrupt.clone()),
            json!({})
        );
        assert_eq!(phases(), expected);
        assert_eq!(f.hook("Interrupt", root, child_turn, interrupt), json!({}));
        expected[2].2 = "closed".to_owned();
        assert_eq!(phases(), expected);
    }
}

#[test]
fn offline_vault_is_recovered_without_user_messages() {
    let f = Fixture::new();
    let hidden = f.vault.with_file_name("offline");
    fs::rename(&f.vault, &hidden).unwrap();
    let start = f.start("offline", "Test the local ownership contract");
    f.stage(token(context(&start)), &batch("Offline quartz"));
    assert_eq!(f.stop("offline", false), json!({}));
    assert_eq!(f.call(&["codex", "diagnostics"], None)["pending"], 1);
    fs::rename(&hidden, &f.vault).unwrap();
    assert_eq!(f.read("quartz"), json!([]));
    let next = f.start("next", "Test another change");
    f.stage(token(context(&next)), &json!({"action":"skip"}));
    assert_eq!(f.stop("next", false), json!({}));
    assert_eq!(f.read("quartz").as_array().unwrap().len(), 1);
    assert_eq!(f.call(&["codex", "diagnostics"], None)["pending"], 0);
}

#[test]
fn ordinary_root_stop_retries_a_durable_operation_from_another_session() {
    let f = Fixture::new();
    let root = "00000000-0000-7000-8000-000000000091";
    let turn = "00000000-0000-7000-8000-000000000092";
    let transcript = f.transcript(root, json!({"id":root,"source":"vscode"}));
    f.timeline(&transcript, turn, "default", None);

    let unstopped = f.hook(
        "UserPromptSubmit",
        "unstopped-session",
        "unstopped",
        json!({"mode":"default","prompt":"Verify spinel"}),
    );
    f.stage(token(context(&unstopped)), &batch("Unclaimed spinel"));
    let interrupted = f.start("interrupted-offline", "Verify fluorite");
    f.stage(token(context(&interrupted)), &batch("Aborted fluorite"));
    f.hook("Interrupt", "session", "interrupted-offline", json!({}));

    let accepted = f.start("accepted-offline", "Verify staurolite");
    f.stage(token(context(&accepted)), &batch("Accepted staurolite"));
    let offline = f.vault.with_file_name("offline");
    fs::rename(&f.vault, &offline).unwrap();
    f.stop("accepted-offline", false);
    assert_eq!(f.call(&["codex", "diagnostics"], None)["pending"], 1);
    fs::rename(&offline, &f.vault).unwrap();

    let stop = json!({
        "transcript_path":transcript,"stop_hook_active":false,
        "last_assistant_message":"The verification is complete."
    });
    let mut plan = stop.clone();
    plan["mode"] = json!("plan");
    assert_eq!(f.hook("Stop", root, turn, plan), json!({}));
    assert_eq!(f.read("staurolite"), json!([]));

    let next = f.hook(
        "UserPromptSubmit",
        root,
        turn,
        json!({"transcript_path":transcript}),
    );
    f.stage(token(context(&next)), &json!({"action":"skip"}));
    let mut read_only = stop.clone();
    read_only["hook_event_name"] = json!("Stop");
    read_only["session_id"] = json!(root);
    read_only["turn_id"] = json!(turn);
    read_only["cwd"] = json!(f.cwd);
    assert_eq!(
        f.call(&["--read-only", "codex", "hook", "Stop"], Some(&read_only)),
        json!({})
    );
    assert_eq!(f.read("staurolite"), json!([]));

    assert_eq!(f.hook("Stop", root, turn, stop), json!({}));
    let hits = f.read("staurolite");
    assert_eq!(hits.as_array().unwrap().len(), 1);
    let record = f.call(&["read", hits[0]["id"].as_str().unwrap()], None);
    assert_eq!(record["session_id"], "session");
    assert_eq!(record["turn_id"], "accepted-offline");
    assert_eq!(f.read("spinel"), json!([]));
    assert_eq!(f.read("fluorite"), json!([]));
}

#[test]
fn forged_transcript_and_unknown_mode_never_publish() {
    let f = Fixture::new();
    let session = "00000000-0000-7000-8000-000000000021";
    let turn = "00000000-0000-7000-8000-000000000022";
    let legitimate = f.transcript(session, json!({"id":session,"source":"vscode"}));
    f.timeline(&legitimate, turn, "default", None);
    let forged = f.cwd.join(legitimate.file_name().unwrap());
    fs::copy(&legitimate, &forged).unwrap();
    let start = f.hook(
        "UserPromptSubmit",
        session,
        turn,
        json!({"prompt":"Check polaris"}),
    );
    f.stage(token(context(&start)), &batch("Unknown polaris"));
    assert_eq!(f.hook("Stop", session, turn, json!({
        "stop_hook_active":false,"transcript_path":forged,"last_assistant_message":"Checked."
    })), json!({}));
    assert_eq!(f.read("polaris"), json!([]));
}

#[test]
fn hard_linked_turn_queue_and_rebound_project_reject_stale_grants() {
    let f = Fixture::new();
    let start = f.start("linked", "Check private queue");
    let grant = token(context(&start));
    let database = f.state.join("codex-turns.sqlite3");
    let link = f.temp.path().join("linked-queue");
    fs::hard_link(&database, &link).unwrap();
    assert!(!f
        .run(
            &["codex", "stage", "--turn", grant],
            Some(&batch("Unsafe queue"))
        )
        .status
        .success());
    fs::remove_file(&link).unwrap();
    f.stage(grant, &batch("Unsafe scope"));
    f.call(
        &[
            "init",
            "--vault",
            f.vault.to_str().unwrap(),
            "--project",
            "new-scope",
        ],
        None,
    );
    assert!(!f
        .run(
            &["codex", "stage", "--turn", grant],
            Some(&batch("Unsafe scope"))
        )
        .status
        .success());
    assert_eq!(f.stop("linked", false), json!({}));
    assert_eq!(f.read("unsafe"), json!([]));
}

#[test]
fn root_turn_can_stage_an_explicit_shared_preference_and_forget_it() {
    let f = Fixture::new();
    let request = json!({"operations":[{"action":"remember","scope":"shared","candidate":{
        "kind":"preference","title":"Meridian results","body":"Include the test result in all project reports.",
        "source":"Explicit user instruction for all projects"
    }}]});
    let start = f.start(
        "shared",
        "For all projects, include the test result in reports.",
    );
    f.stage(token(context(&start)), &request);
    assert_eq!(f.stop("shared", false), json!({}));
    assert_eq!(f.read("meridian"), json!([]));
    let shared = || {
        f.call(
            &[
                "search", "meridian", "--scope", "shared", "--mode", "lexical",
            ],
            None,
        )["hits"]
            .clone()
    };
    let saved = shared();
    let id = saved[0]["id"].as_str().unwrap();
    let start = f.start(
        "forget",
        "Forget that specific shared Meridian results preference.",
    );
    f.stage(
        token(context(&start)),
        &json!({"operations":[{"action":"forget","scope":"shared","id":id}]}),
    );
    assert_eq!(f.stop("forget", false), json!({}));
    assert_eq!(shared(), json!([]));
}

#[test]
fn sandboxed_stage_is_validated_then_accepted_only_by_the_exact_authorized_hook_result() {
    let f = Fixture::new();
    let start = f.start("sandbox", "Verify the restricted workstation convention");
    let text = context(&start);
    let grant = token(text);
    let prefix = text
        .split_once("отдельную буквальную команду ")
        .unwrap()
        .1
        .split_once(" --file 'АБСОЛЮТНЫЙ_ПУТЬ'")
        .unwrap()
        .0;
    let file = f.cwd.join("stage.json");
    fs::write(&file, batch("Restricted peridot").to_string()).unwrap();
    let lock = f
        .state
        .join("codex-turn-locks")
        .join(format!("{}.lock", &grant[..2]));
    let permissions = fs::metadata(&lock).unwrap().permissions();
    let mut readonly = permissions.clone();
    readonly.set_readonly(true);
    fs::set_permissions(&lock, readonly).unwrap();
    let response = f.run(
        &[
            "codex",
            "stage",
            "--turn",
            grant,
            "--file",
            file.to_str().unwrap(),
        ],
        None,
    );
    fs::set_permissions(&lock, permissions).unwrap();
    #[cfg(unix)]
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    assert!(
        response.status.success(),
        "{}",
        String::from_utf8_lossy(&response.stderr)
    );
    let stdout = String::from_utf8(response.stdout).unwrap();
    let receipt: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(receipt["validated"], true);
    assert!(receipt.get("staged").is_none());
    assert_eq!(f.call(&["codex", "diagnostics"], None)["staged"], 0);

    let command = format!("{prefix} --file '{}'", file.display());
    let payload = |command: &str, exit: i32| {
        json!({
            "mode":"default", "tool_name":"Bash", "tool_use_id":"call-stage",
            "tool_input":{"command":command}, "tool_response":stdout, "exit_code":exit
        })
    };
    assert_eq!(
        f.hook(
            "PostToolUse",
            "session",
            "sandbox",
            payload(&format!("{command}; true"), 0)
        ),
        json!({})
    );
    assert_eq!(
        f.hook("PostToolUse", "session", "sandbox", payload(&command, 1)),
        json!({})
    );
    assert_eq!(
        f.hook("PostToolUse", "other", "sandbox", payload(&command, 0)),
        json!({})
    );
    assert_eq!(f.call(&["codex", "diagnostics"], None)["staged"], 0);
    assert_eq!(
        f.hook("PostToolUse", "session", "sandbox", payload(&command, 0)),
        json!({})
    );
    assert_eq!(f.call(&["codex", "diagnostics"], None)["staged"], 1);
    assert_eq!(
        f.hook("PostToolUse", "session", "sandbox", payload(&command, 0)),
        json!({})
    );
    assert_eq!(f.read("peridot"), json!([]));
    assert_eq!(f.stop("sandbox", false), json!({}));
    assert_eq!(f.read("peridot").as_array().unwrap().len(), 1);
}

#[test]
fn desktop_root_retains_capture_grant_after_receiving_a_worker_result() {
    for already_staged in [true, false] {
        let f = Fixture::new();
        let session = "00000000-0000-7000-8000-000000000041";
        let turn = "00000000-0000-7000-8000-000000000042";
        let path = f.transcript(
            session,
            json!({"id":session,"originator":"Codex Desktop","source":"vscode"}),
        );
        f.timeline(&path, turn, "default", None);
        let start = f.hook(
            "UserPromptSubmit",
            session,
            turn,
            json!({
                "transcript_path":path,"prompt":"Remember this team's established review rule."
            }),
        );
        let grant = token(context(&start));
        let request = json!({"operations":[{"action":"remember","candidate":{
            "kind":"preference","title":"Cobalt review checklist","body":"Check reproduction notes before closing.",
            "source":"Explicit project review convention confirmed by the user"
        }}]});
        if already_staged {
            f.stage(grant, &request);
            assert_eq!(f.call(&["codex", "diagnostics"], None)["staged"], 1);
        }
        let mut log = fs::OpenOptions::new().append(true).open(&path).unwrap();
        for event in [
            json!({"type":"inter_agent_communication_metadata","payload":{"trigger_turn":false}}),
            json!({"type":"response_item","payload":{
                "type":"agent_message","author":"/root/worker","recipient":"/root","message":"Verified"
            }}),
        ] {
            writeln!(log, "{event}").unwrap();
        }
        drop(log);
        let stop = |active| {
            f.hook("Stop", session, turn, json!({
            "transcript_path":path,"stop_hook_active":active,"last_assistant_message":"Completed."
        }))
        };
        if already_staged {
            assert_eq!(stop(false), json!({}));
        } else {
            let recovery = stop(false);
            assert_eq!(recovery["decision"], "block");
            let reason = recovery["reason"].as_str().unwrap();
            assert_eq!(token(reason), grant);
            assert!(reason.contains("scope=shared"));
            assert_eq!(stop(false), json!({}));
            f.stage(grant, &request);
            assert_eq!(stop(true), json!({}));
        }
        assert_eq!(f.call(&["codex", "diagnostics"], None)["phase"], "applied");
        assert_eq!(f.read("cobalt").as_array().unwrap().len(), 1);
        assert_eq!(stop(false), json!({}));
        assert_eq!(f.read("cobalt").as_array().unwrap().len(), 1);
    }
}
