use super::*;
use crate::test_directory as tempdir;
use std::{fs, sync::Barrier, thread};
use tempfile::TempDir;

fn fixture() -> (TempDir, Store, Binding) {
    let temp = tempdir().unwrap();
    let vault = temp.path().join("vault");
    let project = temp.path().join("repo");
    fs::create_dir_all(vault.join(".obsidian")).unwrap();
    fs::create_dir(&project).unwrap();
    let store = Store::new(temp.path().join("state")).unwrap();
    let binding = store
        .setup(&vault, &project, "sample", Some("auto"))
        .unwrap();
    (temp, store, binding)
}

fn remember(key: &str) -> Value {
    json!({
        "operation_id": key,
        "action": "remember",
        "candidate": {
            "kind": "decision",
            "title": "Условия выпуска",
            "body": "Check fixture and primary source before release.",
            "source": "Acceptance criteria"
        }
    })
}

fn one(store: &Store, binding: &Binding, item: Value) -> Value {
    commit(store, binding, &json!({"operations": [item]})).unwrap()["receipts"][0].clone()
}

fn start(store: &Store, binding: &Binding, item: &Value) -> (Journal, Operation) {
    let operation = Operation::parse(item).unwrap();
    let mut journal = Journal::open(store, binding, true, true).unwrap().unwrap();
    journal.accept([&operation], (None, None)).unwrap();
    (journal, operation)
}

#[test]
fn replays_after_markdown_publish_without_a_second_write() {
    let (_temp, store, binding) = fixture();
    let (journal, operation) = start(&store, &binding, &remember("test-a"));
    let id = operation.record_id.as_deref().unwrap();
    let path = record(&store.root(&binding).unwrap(), id);
    assert_eq!(
        receipt(&store, &binding, "test-a").unwrap().unwrap()["status"],
        "accepted"
    );
    assert!(!path.exists());

    apply(
        &store,
        &binding,
        &operation.action,
        Some(id),
        None,
        (None, None),
        |_| Ok(()),
    )
    .unwrap();
    let original = fs::read(&path).unwrap();
    assert!(!String::from_utf8_lossy(&original).contains("test-a"));
    drop(journal);

    let result = retry_pending(&store, &binding, 1).unwrap();
    assert_eq!(result["receipts"][0]["status"], "persisted");
    assert_eq!(result["receipts"][0]["record_id"], id);
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(
        one(&store, &binding, remember("test-a")),
        result["receipts"][0]
    );
    assert!(retry_pending(&store, &binding, 1).unwrap()["receipts"]
        .as_array()
        .unwrap()
        .is_empty());

    let journal = Journal::open(&store, &binding, false, false)
        .unwrap()
        .unwrap();
    assert!(journal.entry("test-a").unwrap().unwrap().request.is_none());
    assert!(
        !String::from_utf8_lossy(&fs::read(store.state.join(DATABASE)).unwrap())
            .contains("Check fixture and primary source before release.")
    );
}

#[test]
fn rejects_reused_keys_and_never_implicitly_approves_duplicates() {
    let (_temp, store, binding) = fixture();
    let mut draft = remember("create");
    draft["action"] = json!("draft");
    let original = one(&store, &binding, draft.clone());
    let id = original["record_id"].as_str().unwrap();
    let path = record(&store.root(&binding).unwrap(), id);
    let text = fs::read(&path).unwrap();

    let changed = one(&store, &binding, remember("create"));
    assert_eq!(changed["code"], "operation_id_reused");
    assert_eq!(
        receipt(&store, &binding, "create").unwrap(),
        Some(original.clone())
    );
    let duplicate = one(&store, &binding, remember("second"));
    assert_eq!(duplicate["code"], "record_changed");

    draft["operation_id"] = json!("different-source");
    draft["candidate"]["source"] = json!("Independent source");
    let changed_source = one(&store, &binding, draft);
    assert_eq!(changed_source["record_id"], id);
    assert_eq!(changed_source["code"], "record_changed");
    assert_eq!(fs::read(&path).unwrap(), text);

    let result = one(
        &store,
        &binding,
        json!({"operation_id": "approve", "action": "approve", "id": id}),
    );
    assert_eq!(result["result"]["status"], "memory");
    assert_eq!(store.get(&binding, id).unwrap()["status"], "memory");
}

#[test]
fn batches_report_independent_terminal_conflicts_and_validation_failures() {
    let (_temp, store, binding) = fixture();
    let mut broken_predecessor = remember("missing-parent");
    broken_predecessor["candidate"]["supersedes"] = json!("a".repeat(64));
    let mut invalid_scope = remember("bad-scope");
    invalid_scope["scope"] = json!("shared");
    let mut shared_preference = remember("shared-pref");
    shared_preference["scope"] = json!("shared");
    shared_preference["candidate"]["kind"] = json!("preference");
    let outcome = commit(&store, &binding, &json!({
        "operations": [broken_predecessor, remember("unaffected"), invalid_scope, shared_preference]
    })).unwrap();
    let receipts = outcome["receipts"].as_array().unwrap();
    assert_eq!(receipts[0]["status"], "conflict");
    assert_eq!(receipts[0]["code"], "predecessor_unavailable");
    assert_eq!(receipts[1]["status"], "persisted");
    assert_eq!(receipts[2]["status"], "failed");
    assert_eq!(receipts[3]["result"]["scope"], "shared");
    assert!(receipt(&store, &binding, "bad-scope").unwrap().is_none());
    assert!(retry_pending(&store, &binding, 5).unwrap()["receipts"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .get(&binding, receipts[1]["record_id"].as_str().unwrap())
            .unwrap()["status"],
        "memory"
    );
    assert!(store
        .get(&binding, receipts[3]["record_id"].as_str().unwrap())
        .is_err());
    let shared = shared::for_recall(&store, &binding).unwrap().unwrap();
    assert_eq!(
        store
            .get(&shared, receipts[3]["record_id"].as_str().unwrap())
            .unwrap()["kind"],
        "preference"
    );
}

#[test]
fn accepts_an_unavailable_vault_and_does_not_retarget_pending_operations() {
    let (temp, store, binding) = fixture();
    let vault = binding.root.parent().unwrap();
    let offline = temp.path().join("offline-vault");
    fs::rename(vault, &offline).unwrap();
    let pending = one(&store, &binding, remember("offline"));
    assert_eq!(pending["status"], "retryable");

    let new_vault = temp.path().join("replacement-vault");
    fs::create_dir_all(new_vault.join(".obsidian")).unwrap();
    let rebound = store
        .setup(
            &new_vault,
            &temp.path().join("repo"),
            "sample",
            Some("auto"),
        )
        .unwrap();
    assert_ne!(rebound.generation, binding.generation);
    assert!(receipt(&store, &rebound, "offline").unwrap().is_none());
    assert_eq!(
        retry_pending(&store, &rebound, 5).unwrap()["receipts"],
        json!([])
    );
    assert!(!record(
        &store.root(&rebound).unwrap(),
        pending["record_id"].as_str().unwrap()
    )
    .exists());

    fs::rename(&offline, vault).unwrap();
    let delivered = retry_pending(&store, &binding, 5).unwrap();
    assert_eq!(delivered["receipts"][0]["status"], "persisted");
    assert!(store
        .get(&binding, pending["record_id"].as_str().unwrap())
        .is_ok());
}

#[test]
fn detects_record_changes_before_resuming_approval() {
    let (_temp, store, binding) = fixture();
    let mut draft = remember("draft");
    draft["action"] = json!("draft");
    let saved = one(&store, &binding, draft);
    let id = saved["record_id"].as_str().unwrap();
    let item = json!({"operation_id": "approve", "action": "approve", "id": id});
    let (journal, operation) = start(&store, &binding, &item);
    let interrupted = apply(
        &store,
        &binding,
        &operation.action,
        Some(id),
        None,
        (None, None),
        |version| {
            journal.expect(&operation.id, version)?;
            Err("Simulated interruption".into())
        },
    );
    assert_eq!(interrupted.unwrap_err(), "Simulated interruption");
    drop(journal);

    let path = record(&store.root(&binding).unwrap(), id);
    let text = fs::read_to_string(&path)
        .unwrap()
        .replace("Acceptance criteria", "Unknown source");
    fs::write(&path, &text).unwrap();
    let result = retry_pending(&store, &binding, 5).unwrap();
    assert_eq!(result["receipts"][0]["code"], "record_changed");
    assert_eq!(fs::read_to_string(path).unwrap(), text);
    assert!(store.get(&binding, id).is_err());
}

#[test]
fn refuses_to_approve_a_draft_edited_during_application() {
    let (_temp, store, binding) = fixture();
    let mut draft = remember("draft");
    draft["action"] = json!("draft");
    let saved = one(&store, &binding, draft);
    let id = saved["record_id"].as_str().unwrap();
    let path = record(&store.root(&binding).unwrap(), id);
    let changed = fs::read_to_string(&path)
        .unwrap()
        .replace("Check fixture", "Wait for sign-off");
    let item = json!({"operation_id": "approve", "action": "approve", "id": id});
    let (journal, operation) = start(&store, &binding, &item);
    let result = apply(
        &store,
        &binding,
        &operation.action,
        Some(id),
        None,
        (None, None),
        |version| {
            journal.expect(&operation.id, version)?;
            fs::write(&path, &changed).unwrap();
            Ok(())
        },
    );

    assert_eq!(
        classify(&result.unwrap_err()),
        ("conflict", "record_changed")
    );
    assert_eq!(fs::read_to_string(path).unwrap(), changed);
    assert!(store.get(&binding, id).is_err());
}

#[test]
fn refuses_to_forget_a_record_edited_during_application() {
    let (_temp, store, binding) = fixture();
    let saved = one(&store, &binding, remember("save"));
    let id = saved["record_id"].as_str().unwrap();
    let root = store.root(&binding).unwrap();
    let path = record(&root, id);
    let changed = fs::read_to_string(&path)
        .unwrap()
        .replace("Check fixture", "Wait for sign-off");
    let item = json!({"operation_id": "forget", "action": "forget", "id": id});
    let (journal, operation) = start(&store, &binding, &item);
    let result = apply(
        &store,
        &binding,
        &operation.action,
        Some(id),
        None,
        (None, None),
        |version| {
            journal.expect(&operation.id, version)?;
            fs::write(&path, &changed).unwrap();
            Ok(())
        },
    );

    assert_eq!(
        classify(&result.unwrap_err()),
        ("conflict", "record_changed")
    );
    assert!(!verified_forgotten(&root, id).unwrap());
    assert_eq!(fs::read_to_string(path).unwrap(), changed);
    assert!(store.get(&binding, id).is_ok());
}

#[test]
fn completes_a_removal_interrupted_after_renaming_the_note() {
    let (_temp, store, binding) = fixture();
    let saved = one(&store, &binding, remember("first"));
    let id = saved["record_id"].as_str().unwrap();
    let item = json!({"operation_id": "delete", "action": "forget", "id": id});
    let (journal, operation) = start(&store, &binding, &item);
    let root = store.root(&binding).unwrap();
    assert!(apply(
        &store,
        &binding,
        &operation.action,
        Some(id),
        None,
        (None, None),
        |version| {
            journal.expect(&operation.id, version)?;
            Err("Simulated interruption".into())
        }
    )
    .is_err());
    fs::write(
        root.join("Records").join(format!("{id}-forgotten.json")),
        json!({"id":id,"supersedes":null}).to_string(),
    )
    .unwrap();
    fs::rename(record(&root, id), captured(&root, id)).unwrap();
    drop(journal);

    let outcome = retry_pending(&store, &binding, 1).unwrap();
    assert_eq!(outcome["receipts"][0]["result"]["status"], "forgotten");
    assert!(!captured(&root, id).exists());
    assert!(store.get(&binding, id).is_err());
    let probe = one(&store, &binding, remember("recreate"));
    assert_eq!(probe["code"], "record_forgotten");
}

#[test]
fn serializes_concurrent_same_key_commits() {
    let (_temp, store, binding) = fixture();
    let gate = Barrier::new(2);
    let (first, second) = thread::scope(|scope| {
        let commit = || {
            gate.wait();
            one(&store, &binding, remember("race"))
        };
        let first = scope.spawn(commit);
        let second = scope.spawn(commit);
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_eq!(first, second);
    assert_eq!(first["status"], "persisted");
    let root = store.root(&binding).unwrap();
    assert_eq!(fs::read_dir(root.join("Records")).unwrap().count(), 1);
}

#[test]
fn refuses_to_place_the_durable_journal_inside_the_vault() {
    let (_temp, _store, binding) = fixture();
    let store = Store::new(binding.root.join("local-state")).unwrap();
    let result = commit(
        &store,
        &binding,
        &json!({"operations": [remember("test-a")]}),
    );
    assert!(result.unwrap_err().contains("outside the vault"));
    assert!(!store.state.exists());
}

#[test]
fn replays_with_original_context_after_authorization_ends() {
    let (temp, store, binding) = fixture();
    let vault = binding.root.parent().unwrap();
    let offline = temp.path().join("absent");
    fs::rename(vault, &offline).unwrap();
    let request = json!({"operations": [remember("context")]});
    let submitted = commit_with_context(
        &store,
        &binding,
        &request,
        Some("session-first"),
        Some("turn-first"),
    )
    .unwrap();
    assert_eq!(submitted["receipts"][0]["status"], "retryable");
    fs::rename(offline, vault).unwrap();

    let replay = commit_with_context(
        &store,
        &binding,
        &request,
        Some("session-second"),
        Some("turn-second"),
    )
    .unwrap();
    let result = &replay["receipts"][0];
    assert_eq!(result["status"], "persisted");
    let record = store
        .get(&binding, result["record_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(record["session_id"], "session-first");
    assert_eq!(record["turn_id"], "turn-first");
    let journal = Journal::open(&store, &binding, false, false)
        .unwrap()
        .unwrap();
    assert_eq!(
        journal.entry("context").unwrap().unwrap().context,
        (None, None)
    );
}
