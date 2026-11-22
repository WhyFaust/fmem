use crate::{
    memory::{
        format, io, shared,
        store::{captured, find_record, legacy, verified_forgotten, ForgetSnapshot},
        Binding, Store, WriteOutcome,
    },
    protocol, Result,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};

pub const MAX_BATCH_ITEMS: usize = 64;
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const DATABASE: &str = "operations.sqlite3";

struct Operation {
    id: String,
    action: Value,
    digest: String,
    record_id: Option<String>,
}

impl Operation {
    fn parse(input: &Value) -> Result<Self> {
        let mut fields = input.as_object().ok_or("Invalid memory operation")?.clone();
        let id = fields
            .remove("operation_id")
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or("Missing memory operation ID")?;
        validate_id(&id)?;
        let action = protocol::action(&Value::Object(fields))?;
        let record_id = match action["action"].as_str() {
            Some("remember" | "draft") => {
                Some(format::stable_id(&format::candidate(&action["candidate"])?))
            }
            Some("approve" | "forget") => action["id"].as_str().map(str::to_owned),
            _ => None,
        };
        let digest = io::hash(action.to_string());
        Ok(Self {
            id,
            action,
            digest,
            record_id,
        })
    }

    fn accepted(&self) -> Value {
        let mut receipt = json!({"operation_id": self.id, "status": "accepted"});
        if let Some(id) = &self.record_id {
            receipt["record_id"] = json!(id);
        }
        if self.action["scope"] == "shared" {
            receipt["scope"] = json!("shared");
        }
        receipt
    }
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._:-".contains(&c))
    {
        return Err("Invalid memory operation ID".into());
    }
    Ok(())
}

fn items(request: &Value) -> Result<&[Value]> {
    if request.as_object().is_none_or(|request| request.len() != 1) {
        return Err("Expected a memory operations batch".into());
    }
    let entries = request["operations"]
        .as_array()
        .filter(|entries| (1..=MAX_BATCH_ITEMS).contains(&entries.len()))
        .ok_or("Invalid memory operations batch size")?;
    if request.to_string().len() > MAX_REQUEST_BYTES {
        return Err("Memory operations batch exceeds the size limit".into());
    }
    Ok(entries)
}

pub fn validate_request(request: &Value) -> Result<Value> {
    let mut entries = Vec::new();
    for item in items(request)? {
        let operation = Operation::parse(item)?;
        let mut action = operation.action;
        action["operation_id"] = json!(operation.id);
        entries.push(action);
    }
    Ok(json!({"operations": entries}))
}

fn scope(binding: &Binding) -> Result<String> {
    format::project(&binding.project)?;
    if binding.shared
        || !binding.root.is_absolute()
        || binding.root != io::absolute(&binding.root)?
        || binding
            .root
            .file_name()
            .is_none_or(|name| name != "Codex Memory")
        || !matches!(binding.mode.as_str(), "auto" | "inbox")
        || binding
            .generation
            .as_ref()
            .is_some_and(|g| !format::matches(r"\A[0-9a-f]{32}\z", g))
    {
        return Err("Invalid project binding for memory operations".into());
    }
    Ok(io::hash(
        json!([
            io::normalized(&binding.root),
            binding.project,
            binding.generation,
            binding.mode
        ])
        .to_string(),
    ))
}

fn validate_database_file(path: &Path) -> Result<bool> {
    let Some(metadata) = io::info(path)? else {
        return Ok(false);
    };
    if !metadata.is_file() {
        return Err("Unsafe memory operations database file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err("Unsafe memory operations database file".into());
        }
    }
    #[cfg(windows)]
    if io::windows_info(path, false)?.2 != 1 {
        return Err("Unsafe memory operations database file".into());
    }
    Ok(true)
}

fn db_error(error: rusqlite::Error) -> String {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
            "Memory operations journal is busy".into()
        }
        _ => "Local memory operations journal failed".into(),
    }
}

struct Journal {
    db: Connection,
    scope: String,
}

struct Entry {
    digest: String,
    request: Option<String>,
    expected: Option<String>,
    context: (Option<String>, Option<String>),
    receipt: Value,
}

impl Journal {
    fn open(store: &Store, binding: &Binding, create: bool, write: bool) -> Result<Option<Self>> {
        let scope = scope(binding)?;
        let vault = binding.root.parent().ok_or("Invalid memory root")?;
        if !store.state.is_absolute()
            || store.state != io::absolute(&store.state)?
            || io::within(&store.state, vault)
        {
            return Err("Memory operations state must be local and outside the vault".into());
        }
        for path in store.state.ancestors() {
            if io::directory(&path.join(".obsidian"), false, false)? {
                return Err("Memory operations state must be outside Obsidian vaults".into());
            }
        }
        if !io::profile_state(&store.state, create)? {
            return Ok(None);
        }
        let path = store.state.join(DATABASE);
        if create && !validate_database_file(&path)? {
            match io::options().write(true).create_new(true).open(&path) {
                Ok(_) => io::sync_parent(&path)?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(io::err(error)),
            }
        }
        if !validate_database_file(&path)? {
            return Ok(None);
        }
        validate_database_file(&store.state.join(format!("{DATABASE}-journal")))?;
        for suffix in ["-wal", "-shm"] {
            if io::info(&store.state.join(format!("{DATABASE}{suffix}")))?.is_some() {
                return Err("Unexpected memory operations database files".into());
            }
        }
        let flags = if write {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        };
        let db = Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NOFOLLOW)
            .map_err(db_error)?;
        db.busy_timeout(Duration::from_secs(3)).map_err(db_error)?;
        if write {
            db.execute_batch(
                "PRAGMA synchronous=FULL;
                 PRAGMA secure_delete=ON;
                 CREATE TABLE IF NOT EXISTS operations (
                    sequence INTEGER PRIMARY KEY,
                    scope TEXT NOT NULL,
                    operation_id TEXT NOT NULL,
                    digest TEXT NOT NULL,
                    request TEXT,
                    expected TEXT,
                    session TEXT,
                    turn TEXT,
                    status TEXT NOT NULL CHECK(status IN
                        ('accepted', 'persisted', 'conflict', 'retryable', 'failed')),
                    receipt TEXT NOT NULL,
                    attempts INTEGER NOT NULL DEFAULT 0,
                    UNIQUE(scope, operation_id)
                 );
                 CREATE INDEX IF NOT EXISTS pending_operations
                 ON operations(scope, attempts, sequence) WHERE request IS NOT NULL;",
            )
            .map_err(db_error)?;
        }
        Ok(Some(Self { db, scope }))
    }

    fn entry(&self, key: &str) -> Result<Option<Entry>> {
        let row = self
            .db
            .query_row(
                "SELECT digest, request, expected, session, turn, receipt FROM operations
             WHERE scope=?1 AND operation_id=?2",
                params![self.scope, key],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        (row.get(3)?, row.get(4)?),
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()
            .map_err(db_error)?;
        row.map(|(digest, request, expected, context, receipt)| {
            let receipt: Value =
                serde_json::from_str(&receipt).map_err(|_| "Invalid operation receipt")?;
            if receipt["operation_id"] != key {
                return Err("Invalid operation receipt".into());
            }
            Ok(Entry {
                digest,
                request,
                expected,
                context,
                receipt,
            })
        })
        .transpose()
    }

    fn accept<'a>(
        &mut self,
        operations: impl IntoIterator<Item = &'a Operation>,
        context: (Option<&str>, Option<&str>),
    ) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        for operation in operations {
            tx.execute(
                "INSERT INTO operations(scope, operation_id, digest, request, status, receipt, session, turn)
                 VALUES(?1, ?2, ?3, ?4, 'accepted', ?5, ?6, ?7)
                 ON CONFLICT(scope, operation_id) DO NOTHING",
                params![
                    self.scope,
                    operation.id,
                    operation.digest,
                    operation.action.to_string(),
                    operation.accepted().to_string(),
                    context.0,
                    context.1,
                ],
            )
            .map_err(db_error)?;
        }
        tx.commit().map_err(db_error)
    }

    fn expect(&self, key: &str, version: &str) -> Result<()> {
        self.db
            .execute(
                "UPDATE operations SET expected=?3 WHERE scope=?1 AND operation_id=?2",
                params![self.scope, key, version],
            )
            .map_err(db_error)?;
        Ok(())
    }

    fn finish(&self, key: &str, receipt: &Value) -> Result<()> {
        self.db
            .execute(
                "UPDATE operations SET status=?3, receipt=?4, attempts=attempts+1,
                    request=CASE WHEN ?3='retryable' THEN request ELSE NULL END,
                    expected=CASE WHEN ?3='retryable' THEN expected ELSE NULL END,
                    session=CASE WHEN ?3='retryable' THEN session ELSE NULL END,
                    turn=CASE WHEN ?3='retryable' THEN turn ELSE NULL END
                 WHERE scope=?1 AND operation_id=?2",
                params![
                    self.scope,
                    key,
                    receipt["status"].as_str(),
                    receipt.to_string()
                ],
            )
            .map_err(db_error)?;
        Ok(())
    }

    fn pending(&self, limit: usize) -> Result<Vec<String>> {
        let mut query = self
            .db
            .prepare(
                "SELECT operation_id FROM operations
                 WHERE scope=?1 AND request IS NOT NULL
                 ORDER BY attempts, sequence LIMIT ?2",
            )
            .map_err(db_error)?;
        let keys = query
            .query_map(params![self.scope, limit], |row| row.get(0))
            .map_err(db_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_error)?;
        Ok(keys)
    }
}

pub fn commit(store: &Store, binding: &Binding, request: &Value) -> Result<Value> {
    commit_with_context(store, binding, request, None, None)
}

pub fn commit_with_context(
    store: &Store,
    binding: &Binding,
    request: &Value,
    session: Option<&str>,
    turn: Option<&str>,
) -> Result<Value> {
    let validate_context = |id: &str| format::safe(&json!(id), 128, false);
    let session = session.map(validate_context).transpose()?;
    let turn = turn.map(validate_context).transpose()?;
    let operations = items(request)?
        .iter()
        .map(|item| Operation::parse(item).map_err(|error| invalid_receipt(item, &error)))
        .collect::<Vec<_>>();
    if operations.iter().all(std::result::Result::is_err) {
        let receipts = operations
            .into_iter()
            .filter_map(|entry| entry.err())
            .collect::<Vec<_>>();
        return Ok(json!({"receipts": receipts}));
    }
    let mut journal = Journal::open(store, binding, true, true)?
        .ok_or("Memory operations journal is unavailable")?;
    journal.accept(
        operations.iter().filter_map(|entry| entry.as_ref().ok()),
        (session.as_deref(), turn.as_deref()),
    )?;
    let receipts = operations
        .into_iter()
        .map(|entry| match entry {
            Ok(operation) => advance(
                store,
                binding,
                &journal,
                &operation.id,
                Some(&operation.digest),
                operation.accepted(),
            ),
            Err(receipt) => Ok(receipt),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({"receipts": receipts}))
}

pub fn receipt(store: &Store, binding: &Binding, operation_id: &str) -> Result<Option<Value>> {
    validate_id(operation_id)?;
    let Some(journal) = Journal::open(store, binding, false, false)? else {
        return Ok(None);
    };
    Ok(journal.entry(operation_id)?.map(|entry| entry.receipt))
}

pub fn retry_pending(store: &Store, binding: &Binding, limit: usize) -> Result<Value> {
    if !(1..=MAX_BATCH_ITEMS).contains(&limit) {
        return Err("Invalid memory operations retry limit".into());
    }
    let Some(journal) = Journal::open(store, binding, false, true)? else {
        return Ok(json!({"receipts": []}));
    };
    let mut receipts = Vec::new();
    for key in journal.pending(limit)? {
        let accepted = journal
            .entry(&key)?
            .ok_or("Memory operation receipt is unavailable")?
            .receipt;
        receipts.push(advance(store, binding, &journal, &key, None, accepted)?);
    }
    Ok(json!({"receipts": receipts}))
}

fn invalid_receipt(item: &Value, error: &str) -> Value {
    let mut receipt = json!({"status": "failed", "code": "invalid_request", "message": error});
    if let Some(id) = item["operation_id"]
        .as_str()
        .filter(|id| validate_id(id).is_ok())
    {
        receipt["operation_id"] = json!(id);
    }
    receipt
}

fn advance(
    store: &Store,
    binding: &Binding,
    journal: &Journal,
    key: &str,
    request_digest: Option<&str>,
    mut fallback: Value,
) -> Result<Value> {
    let key_hash = io::hash(json!([io::normalized(&store.state), journal.scope]).to_string());
    let _lock = match io::lock_key(&key_hash, binding.root.parent()) {
        Ok(lock) => lock,
        Err(error) => {
            let entry = journal
                .entry(key)?
                .ok_or("Memory operation receipt is unavailable")?;
            if request_digest.is_some_and(|digest| digest != entry.digest) {
                return Ok(key_conflict(fallback));
            }
            if entry.request.is_none() {
                return Ok(entry.receipt);
            }
            fallback["code"] = json!("journal_lock");
            fallback["message"] = json!(error);
            return Ok(fallback);
        }
    };
    let entry = journal
        .entry(key)?
        .ok_or("Memory operation receipt is unavailable")?;
    if request_digest.is_some_and(|digest| digest != entry.digest) {
        return Ok(key_conflict(fallback));
    }
    let Some(data) = entry.request else {
        return Ok(entry.receipt);
    };
    let result = serde_json::from_str(&data)
        .map_err(|_| "Invalid memory operations journal request".to_owned())
        .and_then(|data| {
            apply(
                store,
                binding,
                &data,
                entry.receipt["record_id"].as_str(),
                entry.expected.as_deref(),
                (entry.context.0.as_deref(), entry.context.1.as_deref()),
                |version| journal.expect(key, version),
            )
        });
    let mut receipt = entry.receipt;
    for field in ["code", "message", "result"] {
        receipt.as_object_mut().unwrap().remove(field);
    }
    match result {
        Ok(outcome) => {
            receipt["status"] = json!("persisted");
            receipt["result"] = outcome;
        }
        Err(error) => {
            let (status, code) = classify(&error);
            receipt["status"] = json!(status);
            receipt["code"] = json!(code);
            receipt["message"] = json!(error);
        }
    }
    if let Err(error) = journal.finish(key, &receipt) {
        fallback["code"] = json!("journal_ack");
        fallback["message"] = json!(error);
        return Ok(fallback);
    }
    Ok(receipt)
}

fn key_conflict(mut receipt: Value) -> Value {
    receipt["status"] = json!("conflict");
    receipt["code"] = json!("operation_id_reused");
    receipt["message"] = json!("Memory operation ID was accepted with a different request");
    receipt
}

fn classify(error: &str) -> (&'static str, &'static str) {
    match error {
        "Memory entry was forgotten" => ("conflict", "record_forgotten"),
        "Superseded memory entry is unavailable" => ("conflict", "predecessor_unavailable"),
        "Memory record already exists"
        | "Existing memory entry differs from the requested result"
        | "Memory entry changed after operation acceptance"
        | "Memory note changed while preparing an update"
        | "Memory tombstone changed while forgetting an entry" => ("conflict", "record_changed"),
        "Memory entry is unavailable"
        | "Inbox entry is unavailable"
        | "Shared preferences are unavailable" => ("conflict", "record_unavailable"),
        "Local memory operation failed"
        | "Local memory operations journal failed"
        | "Memory operations journal is busy"
        | "Memory vault is unavailable"
        | "Memory project unavailable"
        | "Missing memory directory"
        | "Timed out waiting for the local memory lock" => ("retryable", "temporarily_unavailable"),
        _ => ("failed", "memory_operation_failed"),
    }
}

fn apply(
    store: &Store,
    project: &Binding,
    data: &Value,
    record_id: Option<&str>,
    expected: Option<&str>,
    context: (Option<&str>, Option<&str>),
    remember_version: impl FnOnce(&str) -> Result<()>,
) -> Result<Value> {
    let action = protocol::action(data)?;
    let verb = action["action"].as_str().ok_or("Invalid memory action")?;
    if verb == "skip" {
        return Ok(json!({"status": "skipped"}));
    }
    let id = format::id(record_id.ok_or("Missing memory operation record ID")?)?;
    if !io::file(&project.root.join("Home.md"))? {
        return Err("Memory vault is unavailable".into());
    }
    let scoped;
    let binding = if action["scope"] == "shared" {
        scoped = shared::for_action(store, project, &action)?;
        &scoped
    } else {
        project
    };
    let root = store.root(binding)?;
    if !io::directory(&root.join("Records"), false, false)? {
        return Err("Memory project unavailable".into());
    }
    let _lock = io::lock(&root, binding.root.parent().ok_or("Invalid memory root")?)?;
    let forgotten = verified_forgotten(&root, id)?;
    if forgotten && verb != "forget" {
        return Err("Memory entry was forgotten".into());
    }
    let path = find_record(&root, id)?;
    let raw = path.as_deref().map(io::read).transpose()?.flatten();
    let capture = if verb == "forget" {
        io::read(&captured(&root, id))?
    } else {
        None
    };
    if capture.is_some() && (!forgotten || raw.is_some() && raw != capture) {
        return Err("Memory note changed while preparing an update".into());
    }
    let fields = raw
        .as_deref()
        .or(capture.as_deref())
        .map(|note| format::parse(note, id, format::MEMORY, true))
        .transpose()?;
    let mut status = if let Some(fields) = fields {
        let (approvals, hidden) = store.statuses(&root)?;
        if verb != "forget" && hidden.contains(id) {
            return Err("Memory entry is unavailable".into());
        }
        let inline = format::inline(&fields);
        Some(format::public(
            fields,
            approvals.get(id).or(inline.as_ref()),
        ))
    } else {
        None
    };
    if let Some(record) = &status {
        if binding.shared && record["kind"] != "preference" {
            return Err(
                "Only an explicitly shared preference can be saved in shared memory".into(),
            );
        }
    }
    let result = match verb {
        "remember" | "draft" => {
            let candidate = format::candidate(&action["candidate"])?;
            if format::stable_id(&candidate) != id {
                return Err("Invalid memory operation record ID".into());
            }
            let approve = verb == "remember";
            let desired = if approve && binding.mode == "auto" {
                "memory"
            } else {
                "inbox"
            };
            if let Some(record) = status {
                if record["status"] != desired
                    || ["kind", "title", "body", "source", "supersedes"]
                        .iter()
                        .any(|key| record[*key].as_str() != candidate.get(*key).map(String::as_str))
                    || [("session_id", context.0), ("turn_id", context.1)]
                        .into_iter()
                        .any(|(key, value)| value.is_some() && record[key].as_str() != value)
                {
                    return Err("Existing memory entry differs from the requested result".into());
                }
                io::sync_parent(path.as_deref().ok_or("Memory entry is unavailable")?)?;
                record
            } else {
                write_result(
                    store.save_locked(binding, &root, &candidate, approve, context, false)?,
                )?
            }
        }
        "approve" => {
            let observed = raw.as_deref().ok_or("Inbox entry is unavailable")?;
            let mut record = status.take().ok_or("Inbox entry is unavailable")?;
            let approved =
                record.as_object_mut().unwrap().remove("status") == Some(json!("memory"));
            verify_version(expected, &io::hash(record.to_string()), remember_version)?;
            if approved {
                let path = path.as_deref().ok_or("Memory entry is unavailable")?;
                if io::read(path)?.as_deref() != Some(observed) {
                    return Err("Memory note changed while preparing an update".into());
                }
                io::sync_parent(path)?;
                record["status"] = json!("memory");
                record
            } else {
                write_result(store.approve_locked(&root, id, Some(observed))?)?
            }
        }
        "forget" => {
            let approval = io::read(&legacy(&root, "Approved", id))?;
            if let Some(record) = &status {
                verify_version(expected, &io::hash(record.to_string()), remember_version)?;
            } else if !forgotten {
                if binding.shared {
                    return Err("Memory entry is unavailable".into());
                }
                let marker = approval.as_deref().ok_or("Memory entry is unavailable")?;
                let old = format::parse(marker, id, format::APPROVED, true)?;
                verify_version(
                    expected,
                    &io::hash(json!(old).to_string()),
                    remember_version,
                )?;
            }
            store.forget_locked(
                &root,
                id,
                Some(ForgetSnapshot {
                    record: raw.as_deref(),
                    captured: capture.as_deref(),
                    approval_marker: approval.as_deref(),
                }),
            )?
        }
        _ => return Err("Invalid memory action".into()),
    };
    let mut simple = json!({"id": result["id"], "status": result["status"]});
    if binding.shared {
        simple["scope"] = json!("shared");
    }
    Ok(simple)
}

fn verify_version(
    expected: Option<&str>,
    observed: &str,
    remember: impl FnOnce(&str) -> Result<()>,
) -> Result<()> {
    match expected {
        Some(version) if version != observed => {
            Err("Memory entry changed after operation acceptance".into())
        }
        Some(_) => Ok(()),
        None => remember(observed),
    }
}

fn write_result(outcome: WriteOutcome) -> Result<Value> {
    match outcome {
        WriteOutcome::Written(record) => Ok(record),
        WriteOutcome::Forgotten => Err("Memory entry was forgotten".into()),
    }
}

#[cfg(test)]
#[path = "operations_tests.rs"]
mod tests;
