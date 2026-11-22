use crate::{
    embeddings::{Embeddings, DIMENSIONS},
    memory::{
        io,
        store::{self, Binding, Store},
    },
    Result,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};
use rust_stemmers::{Algorithm, Stemmer};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::Path,
    sync::{LazyLock, OnceLock},
    time::Duration,
};
use unicode_casefold::UnicodeCaseFold;

static STOP_WORDS: LazyLock<BTreeSet<&str>> = LazyLock::new(|| {
    concat!(
        "a an and are as at be been but by can could did do does for from had has have how i ",
        "if in into is it its me my of on or our should that the their this to us was we were ",
        "what when where which who why will with would you your а без был была были было в во ",
        "вот для до его ее её если есть же за и из или к как какая какие какой какое ко когда ",
        "кто ли мне мой мы на над нам наш не ни но о об от по под при про с со так там то тот ",
        "у уже что чтобы это этого этой этом этот я",
    )
    .split_whitespace()
    .collect()
});
static WORD: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"[\p{L}\p{N}]+").expect("valid word pattern"));
static STEMMER: LazyLock<Stemmer> = LazyLock::new(|| Stemmer::create(Algorithm::Russian));
static VECTOR_EXTENSION: OnceLock<i32> = OnceLock::new();

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS headers (
        id TEXT PRIMARY KEY,
        stamp TEXT NOT NULL,
        approval TEXT NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS documents (
        rowid INTEGER PRIMARY KEY AUTOINCREMENT,
        id TEXT NOT NULL UNIQUE,
        stamp TEXT NOT NULL,
        approval TEXT NOT NULL,
        digest TEXT NOT NULL,
        revision TEXT NOT NULL,
        data TEXT NOT NULL,
        created TEXT,
        kind TEXT NOT NULL
    );
    CREATE VIRTUAL TABLE IF NOT EXISTS lexical USING fts5(title, body, forms);
    CREATE VIRTUAL TABLE IF NOT EXISTS semantic USING vec0(embedding float[384] distance_metric=cosine);
    INSERT INTO lexical(lexical,rank) VALUES('secure-delete',1);
    PRAGMA user_version=1;
";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchMode {
    Lexical,
    Semantic,
    Hybrid,
}

#[derive(Clone, Debug, Serialize)]
pub struct SearchHit {
    pub id: String,
    pub source: String,
    pub revision: String,
    pub title: String,
    pub kind: String,
    pub scope: String,
    pub snippet: String,
    pub score: f64,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    pub semantic_unavailable: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct IndexReport {
    pub active: usize,
    pub updated: usize,
    pub removed: usize,
    pub pending: usize,
    pub embedded: usize,
}

struct SearchIndex {
    connection: Connection,
}

#[derive(Clone, Copy, PartialEq)]
enum Refresh {
    Incremental,
    Reconcile,
    Rebuild,
}

struct Header {
    stamp: String,
    approval: String,
}

struct DocumentVersions {
    stamp: String,
    approval: String,
    digest: String,
    rowid: i64,
}

struct Pending {
    rowid: i64,
    revision: String,
    text: String,
}

struct Ranked {
    rowid: i64,
    score: f64,
}

impl Store {
    pub(crate) fn ranked_records(
        &self,
        binding: &Binding,
        query: &str,
        limit: usize,
        fill: bool,
    ) -> Result<Value> {
        self.ranked_records_with_vector(binding, query, limit, fill, None)
    }

    pub fn recall_with_vector(
        &self,
        binding: &Binding,
        query: &str,
        limit: usize,
        vector: &[f32],
    ) -> Result<Value> {
        self.ranked_records_with_vector(binding, query, limit, true, Some(vector))
    }

    fn ranked_records_with_vector(
        &self,
        binding: &Binding,
        query: &str,
        limit: usize,
        fill: bool,
        vector: Option<&[f32]>,
    ) -> Result<Value> {
        validate(query, limit, None)?;
        let query = LexicalQuery::new(query);
        if query.expression.is_none() && !fill {
            return Ok(Value::Array(Vec::new()));
        }
        let root = self.root(binding)?;
        let vault = binding.root.parent().ok_or("Missing parent vault")?;
        let _lock = if fill {
            io::lock_with_timeout(&root, vault, Duration::ZERO)?
        } else {
            io::lock(&root, vault)?
        };
        let mut index = SearchIndex::open(&self.state, &root, vault, false)?;
        index.refresh(self, &root, Refresh::Incremental, !fill)?;
        let mut ranked = if let Some(vector) = vector {
            let candidates = (4 * limit).max(32);
            let lexical = index.lexical(&query, candidates, None)?;
            let semantic = index.semantic(vector, candidates, None)?;
            let mut hits = reciprocal_rank_fusion(lexical, semantic);
            hits.truncate(limit);
            hits
        } else {
            index.lexical(&query, limit, None)?
        };
        if fill && ranked.len() < limit {
            index.fill_with_rules(&mut ranked, limit)?;
        }
        let records = ranked
            .iter()
            .map(|hit| index.document(hit.rowid).map(|(_, value)| value))
            .collect::<Result<Vec<_>>>()?;
        Ok(Value::Array(records))
    }

    pub fn search_compact(
        &self,
        binding: &Binding,
        query: &str,
        limit: usize,
        mode: SearchMode,
        kind: Option<&str>,
        embeddings: Option<&mut Embeddings>,
    ) -> Result<SearchResponse> {
        validate(query, limit, kind)?;
        let lexical_query = LexicalQuery::new(query);
        let mut semantic_unavailable = None;
        let vector = if mode == SearchMode::Lexical || query.trim().is_empty() {
            None
        } else {
            match embeddings.map(|model| model.query(query)) {
                Some(Ok(vector)) => Some(vector),
                Some(Err(error)) => {
                    semantic_unavailable = Some(error);
                    None
                }
                None => {
                    semantic_unavailable = Some("Local embedding model is unavailable".into());
                    None
                }
            }
        };
        let root = self.root(binding)?;
        let vault = binding.root.parent().ok_or("Missing parent vault")?;
        let _lock = io::lock(&root, vault)?;
        let mut index = SearchIndex::open(&self.state, &root, vault, false)?;
        index.refresh(self, &root, Refresh::Incremental, false)?;
        let candidates = if mode == SearchMode::Hybrid && vector.is_some() {
            (4 * limit).max(32)
        } else {
            limit
        };
        let lexical = if mode != SearchMode::Semantic && query.trim().is_empty() && kind.is_some() {
            index.recent(kind, limit)?
        } else if mode == SearchMode::Semantic {
            Vec::new()
        } else {
            index.lexical(&lexical_query, candidates, kind)?
        };
        let semantic = match vector {
            Some(vector) => {
                let matches = index.semantic(&vector, candidates, kind)?;
                let pending = index.pending_count(kind)?;
                if pending > 0 {
                    semantic_unavailable = Some(format!(
                        "Semantic index is incomplete ({pending} pending in scope). Run fmem index --semantic"
                    ));
                }
                matches
            }
            None => Vec::new(),
        };
        let ranked = match mode {
            SearchMode::Hybrid => reciprocal_rank_fusion(lexical, semantic),
            SearchMode::Semantic => semantic,
            SearchMode::Lexical => lexical,
        };
        let scope = if binding.shared {
            "shared"
        } else {
            &binding.project
        };
        let hits = ranked
            .into_iter()
            .take(limit)
            .map(|hit| {
                let (revision, record) = index.document(hit.rowid)?;
                let id = record["id"].as_str().ok_or("Missing indexed record ID")?;
                let source = store::record(&root, id)?;
                let source = source.strip_prefix(vault).map_err(io::err)?;
                let source = source
                    .iter()
                    .map(|part| part.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                Ok(SearchHit {
                    id: id.into(),
                    source,
                    revision,
                    title: field(&record, "title")?.into(),
                    kind: field(&record, "kind")?.into(),
                    scope: scope.into(),
                    snippet: lexical_query.snippet(field(&record, "body")?),
                    score: hit.score,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(SearchResponse {
            hits,
            semantic_unavailable,
        })
    }

    pub fn index(
        &self,
        binding: &Binding,
        rebuild: bool,
        embeddings: Option<&mut Embeddings>,
    ) -> Result<IndexReport> {
        let mode = if rebuild {
            Refresh::Rebuild
        } else {
            Refresh::Reconcile
        };
        self.update(binding, mode, embeddings, None, || true)
    }

    pub fn update_index(
        &self,
        binding: &Binding,
        embeddings: Option<&mut Embeddings>,
    ) -> Result<IndexReport> {
        self.update(binding, Refresh::Incremental, embeddings, None, || true)
    }

    pub fn update_index_batch(
        &self,
        binding: &Binding,
        model: &mut Embeddings,
        limit: usize,
        still_bound: impl FnOnce() -> bool,
    ) -> Result<IndexReport> {
        if !(1..=32).contains(&limit) {
            return Err("Invalid embedding batch limit".into());
        }
        self.update(
            binding,
            Refresh::Incremental,
            Some(model),
            Some(limit),
            still_bound,
        )
    }

    fn update(
        &self,
        binding: &Binding,
        mode: Refresh,
        embeddings: Option<&mut Embeddings>,
        batch_limit: Option<usize>,
        still_bound: impl FnOnce() -> bool,
    ) -> Result<IndexReport> {
        let root = self.root(binding)?;
        let vault = binding.root.parent().ok_or("Missing parent vault")?;
        let timeout = if batch_limit.is_some() {
            Duration::ZERO
        } else {
            Duration::from_secs(3)
        };
        let (mut index, mut report) = {
            let _lock = io::lock_with_timeout(&root, vault, timeout)?;
            let mut index = SearchIndex::open(&self.state, &root, vault, mode == Refresh::Rebuild)?;
            let mut report = index.refresh(self, &root, mode, false)?;
            report.pending = index.pending_count(None)?;
            (index, report)
        };
        let Some(model) = embeddings.filter(|_| report.pending > 0) else {
            return Ok(report);
        };
        let mut completed = Vec::new();
        let mut after = 0;
        loop {
            let batch = index.pending(after, batch_limit.unwrap_or(32))?;
            if batch.is_empty() {
                break;
            }
            after = batch.last().map_or(after, |candidate| candidate.rowid);
            let passages = batch
                .iter()
                .map(|pending| pending.text.clone())
                .collect::<Vec<_>>();
            let vectors = model.passages(&passages)?;
            completed.extend(
                batch
                    .into_iter()
                    .zip(vectors)
                    .map(|(pending, vector)| (pending.rowid, pending.revision, vector)),
            );
            if batch_limit.is_some() {
                break;
            }
        }
        if !still_bound() {
            return Err("Memory binding changed during embedding".into());
        }
        let _lock = io::lock_with_timeout(&root, vault, timeout)?;
        index = SearchIndex::open(&self.state, &root, vault, false)?;
        let rechecked = index.refresh(self, &root, Refresh::Incremental, false)?;
        report.updated += rechecked.updated;
        report.removed += rechecked.removed;
        report.active = rechecked.active;
        report.embedded = index.commit_vectors(completed)?;
        report.pending = index.pending_count(None)?;
        Ok(report)
    }
}

impl SearchIndex {
    fn open(state: &Path, root: &Path, vault: &Path, rebuild: bool) -> Result<Self> {
        let directory = state.join("indexes");
        if io::within(&directory, vault) {
            return Err("Search index directory must be outside the Obsidian vault".into());
        }
        io::profile_state(&directory, true)?;
        let path = directory.join(format!("{}-v1.sqlite3", io::hash(io::directory_key(root))));
        register_vector_extension()?;
        match Self::open_path(&path) {
            Ok(mut index) => {
                if rebuild {
                    Self::initialize_schema(&mut index.connection, true)?;
                }
                Ok(index)
            }
            Err(error) if rebuild && io::file(&path)? => {
                for suffix in ["-journal", "-wal", "-shm", ""] {
                    let file = path.with_file_name(format!(
                        "{}{suffix}",
                        path.file_name()
                            .ok_or("Missing index file name")?
                            .to_string_lossy()
                    ));
                    if io::file(&file)? {
                        fs::remove_file(&file).map_err(io::err)?;
                    }
                }
                Self::open_path(&path).map_err(|_| error)
            }
            result => result,
        }
    }

    fn open_path(path: &Path) -> Result<Self> {
        if !io::file(path)? {
            match io::options().create_new(true).write(true).open(path) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(io::err(error)),
            }
        }
        let mut connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(database_error)?;
        connection
            .busy_timeout(Duration::from_secs(3))
            .map_err(database_error)?;
        connection
            .pragma_update(None, "secure_delete", "ON")
            .map_err(database_error)?;
        let schema_version = connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i32>(0))
            .map_err(database_error)?;
        if schema_version == 0 {
            Self::initialize_schema(&mut connection, false)?;
        }
        Ok(Self { connection })
    }

    fn initialize_schema(connection: &mut Connection, rebuild: bool) -> Result<()> {
        let transaction = connection.transaction().map_err(database_error)?;
        if rebuild {
            transaction.execute_batch("DROP TABLE IF EXISTS semantic; DROP TABLE IF EXISTS lexical; DROP TABLE IF EXISTS documents; DROP TABLE IF EXISTS headers;")
                .map_err(database_error)?;
        }
        transaction.execute_batch(SCHEMA).map_err(database_error)?;
        transaction.commit().map_err(database_error)
    }

    fn refresh(
        &mut self,
        store: &Store,
        root: &Path,
        mode: Refresh,
        warn: bool,
    ) -> Result<IndexReport> {
        let records = store::records(root)?;
        let mut stamps = BTreeMap::new();
        for record in &records {
            if let Some(stamp) = fingerprint(record)? {
                stamps.insert(store::record_id(record)?.to_owned(), stamp);
            }
        }

        let transaction = self.connection.transaction().map_err(database_error)?;
        let full = mode != Refresh::Incremental;
        let mut headers = indexed_headers(&transaction)?;
        let (approvals, hidden) = store.record_statuses_with(root, records, |path, id| {
            let Some(stamp) = stamps.get(id) else {
                return Ok(None);
            };
            if !full {
                if let Some(header) = headers.get(id).filter(|header| &header.stamp == stamp) {
                    if let Some(approval) =
                        serde_json::from_str(&header.approval).map_err(io::err)?
                    {
                        return Ok(Some(approval));
                    }
                }
            }
            let approval = store::record_inline_status(path, id)?;
            let encoded = serde_json::to_string(&approval).map_err(io::err)?;
            if headers
                .get(id)
                .is_none_or(|header| header.approval != encoded || &header.stamp != stamp)
            {
                transaction
                    .execute(
                        "INSERT INTO headers VALUES (?1,?2,?3) ON CONFLICT(id)
                     DO UPDATE SET stamp=excluded.stamp, approval=excluded.approval",
                        params![id, stamp, encoded],
                    )
                    .map_err(database_error)?;
                headers.insert(
                    id.to_owned(),
                    Header {
                        stamp: stamp.clone(),
                        approval: encoded,
                    },
                );
            }
            Ok(approval)
        })?;
        for id in headers.keys().filter(|id| !stamps.contains_key(*id)) {
            transaction
                .execute("DELETE FROM headers WHERE id=?", [id])
                .map_err(database_error)?;
        }

        let mut versions = document_versions(&transaction)?;
        let mut report = IndexReport::default();
        for (id, approval) in approvals.iter().filter(|(id, _)| !hidden.contains(*id)) {
            let Some(stamp) = stamps.get(id) else {
                continue;
            };
            let previous = versions.remove(id);
            let approval_hash = io::hash(serde_json::to_vec(approval).map_err(io::err)?);
            if previous
                .as_ref()
                .is_some_and(|old| !full && old.stamp == *stamp && old.approval == approval_hash)
            {
                report.active += 1;
                continue;
            }
            let Some(record) = store.search_record(root, id, approval, warn)? else {
                if let Some(previous) = previous {
                    delete_document(&transaction, previous.rowid)?;
                    report.removed += 1;
                }
                continue;
            };
            let title = field(&record, "title")?;
            let body = field(&record, "body")?;
            let digest = io::hash(serde_json::to_vec(&(title, body)).map_err(io::err)?);
            let data = serde_json::to_string(&record).map_err(io::err)?;
            let revision = io::hash(data.as_bytes());
            transaction.execute(
                "INSERT INTO documents(id,stamp,approval,digest,revision,data,created,kind)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(id) DO UPDATE SET
                 stamp=excluded.stamp, approval=excluded.approval, digest=excluded.digest,
                 revision=excluded.revision, data=excluded.data, created=excluded.created, kind=excluded.kind
                 WHERE stamp<>excluded.stamp OR approval<>excluded.approval OR revision<>excluded.revision",
                params![id, stamp, approval_hash, digest, revision, data, record["created"].as_str(), field(&record, "kind")?],
            ).map_err(database_error)?;
            if previous.as_ref().is_none_or(|old| old.digest != digest) {
                let rowid = transaction
                    .query_row("SELECT rowid FROM documents WHERE id=?", [id], |row| {
                        row.get::<_, i64>(0)
                    })
                    .map_err(database_error)?;
                if previous.is_some() {
                    delete_search_data(&transaction, rowid)?;
                }
                let title = title.case_fold().collect::<String>();
                let body = body.case_fold().collect::<String>();
                let forms = WORD
                    .find_iter(&title)
                    .chain(WORD.find_iter(&body))
                    .filter_map(|term| russian_stem(term.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ");
                transaction
                    .execute(
                        "INSERT INTO lexical(rowid,title,body,forms) VALUES(?,?,?,?)",
                        params![rowid, title, body, forms],
                    )
                    .map_err(database_error)?;
                report.updated += 1;
            }
            report.active += 1;
        }
        for obsolete in versions.values() {
            delete_document(&transaction, obsolete.rowid)?;
            report.removed += 1;
        }
        transaction.commit().map_err(database_error)?;
        Ok(report)
    }

    fn lexical(
        &self,
        query: &LexicalQuery,
        limit: usize,
        kind: Option<&str>,
    ) -> Result<Vec<Ranked>> {
        let Some(expression) = &query.expression else {
            return Ok(Vec::new());
        };
        let mut statement = self
            .connection
            .prepare(
                "SELECT documents.rowid, -bm25(lexical,2,1,0.35) AS score
             FROM lexical JOIN documents ON documents.rowid=lexical.rowid
             WHERE lexical MATCH ?1 AND (?2 IS NULL OR documents.kind=?2)
             ORDER BY score DESC, julianday(documents.created) DESC, documents.id LIMIT ?3",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map(params![expression, kind, limit], ranked)
            .map_err(database_error)?;
        rows.collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }

    fn semantic(&self, vector: &[f32], limit: usize, kind: Option<&str>) -> Result<Vec<Ranked>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT nearest.rowid, 1.0-nearest.distance
             FROM (
                 SELECT rowid, distance FROM semantic WHERE embedding MATCH ?1 AND k=?2
                 AND rowid IN (SELECT rowid FROM documents WHERE ?3 IS NULL OR kind=?3)
                 ORDER BY distance
             ) AS nearest JOIN documents ON documents.rowid=nearest.rowid
             ORDER BY nearest.distance, documents.id",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map(params![vector_blob(vector)?, limit, kind], ranked)
            .map_err(database_error)?;
        rows.collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }

    fn document(&self, rowid: i64) -> Result<(String, Value)> {
        let (revision, data): (String, String) = self
            .connection
            .query_row(
                "SELECT revision,data FROM documents WHERE rowid=?",
                [rowid],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(database_error)?;
        Ok((revision, serde_json::from_str(&data).map_err(io::err)?))
    }

    fn recent(&self, kind: Option<&str>, limit: usize) -> Result<Vec<Ranked>> {
        let mut statement = self.connection.prepare(
            "SELECT rowid, 0.0 FROM documents WHERE kind=?1 ORDER BY julianday(created) DESC,id LIMIT ?2",
        ).map_err(database_error)?;
        let rows = statement
            .query_map(params![kind, limit], ranked)
            .map_err(database_error)?;
        rows.collect::<rusqlite::Result<_>>()
            .map_err(database_error)
    }

    fn fill_with_rules(&self, ranked: &mut Vec<Ranked>, limit: usize) -> Result<()> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT rowid FROM documents WHERE kind IN ('preference','correction')
             ORDER BY created DESC,id LIMIT ?",
            )
            .map_err(database_error)?;
        let candidate_rows = statement
            .query_map([limit], |row| row.get(0))
            .map_err(database_error)?;
        for candidate in candidate_rows {
            let rowid = candidate.map_err(database_error)?;
            if !ranked.iter().any(|hit| hit.rowid == rowid) {
                ranked.push(Ranked { rowid, score: 0.0 });
            }
            if ranked.len() == limit {
                break;
            }
        }
        Ok(())
    }

    fn pending_count(&self, kind: Option<&str>) -> Result<usize> {
        self.connection
            .query_row(
                "SELECT count(*) FROM documents WHERE (?1 IS NULL OR kind=?1)
             AND rowid NOT IN (SELECT rowid FROM semantic)",
                [kind],
                |row| row.get(0),
            )
            .map_err(database_error)
    }

    fn pending(&self, after: i64, limit: usize) -> Result<Vec<Pending>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT rowid,revision,data FROM documents WHERE rowid>? AND
             rowid NOT IN (SELECT rowid FROM semantic) ORDER BY rowid LIMIT ?2",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map(params![after, limit], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(database_error)?;
        rows.map(|row| {
            let (rowid, revision, data) = row.map_err(database_error)?;
            let record = serde_json::from_str::<Value>(&data).map_err(io::err)?;
            let text = format!("{}\n{}", field(&record, "title")?, field(&record, "body")?);
            Ok(Pending {
                rowid,
                revision,
                text,
            })
        })
        .collect()
    }

    fn commit_vectors(&mut self, vectors: Vec<(i64, String, Vec<f32>)>) -> Result<usize> {
        let transaction = self.connection.transaction().map_err(database_error)?;
        let mut committed = 0;
        {
            let mut current = transaction.prepare(
                "SELECT revision FROM documents WHERE rowid=? AND rowid NOT IN (SELECT rowid FROM semantic)",
            ).map_err(database_error)?;
            let mut insert = transaction
                .prepare("INSERT INTO semantic(rowid,embedding) VALUES(?,?)")
                .map_err(database_error)?;
            for (rowid, revision, vector) in vectors {
                let current_revision = current
                    .query_row([rowid], |row| row.get::<_, String>(0))
                    .optional()
                    .map_err(database_error)?;
                if current_revision.as_deref() == Some(&revision) {
                    insert
                        .execute(params![rowid, vector_blob(&vector)?])
                        .map_err(database_error)?;
                    committed += 1;
                }
            }
        }
        transaction.commit().map_err(database_error)?;
        Ok(committed)
    }
}

fn register_vector_extension() -> Result<()> {
    let status = VECTOR_EXTENSION.get_or_init(|| unsafe {
        use rusqlite::ffi;
        ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
            unsafe extern "C" fn(),
            unsafe extern "C" fn(
                *mut ffi::sqlite3,
                *mut *mut std::ffi::c_char,
                *const ffi::sqlite3_api_routines,
            ) -> i32,
        >(sqlite_vec::sqlite3_vec_init)))
    });
    if *status != rusqlite::ffi::SQLITE_OK {
        return Err("Could not register sqlite-vec".into());
    }
    Ok(())
}

fn database_error(error: rusqlite::Error) -> String {
    format!("Local search index operation failed: {error}")
}

fn indexed_headers(transaction: &Transaction) -> Result<HashMap<String, Header>> {
    let mut statement = transaction
        .prepare("SELECT id,stamp,approval FROM headers")
        .map_err(database_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                Header {
                    stamp: row.get(1)?,
                    approval: row.get(2)?,
                },
            ))
        })
        .map_err(database_error)?;
    rows.collect::<rusqlite::Result<_>>()
        .map_err(database_error)
}

fn document_versions(transaction: &Transaction) -> Result<BTreeMap<String, DocumentVersions>> {
    let mut statement = transaction
        .prepare("SELECT id,stamp,approval,digest,rowid FROM documents")
        .map_err(database_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                DocumentVersions {
                    stamp: row.get(1)?,
                    approval: row.get(2)?,
                    digest: row.get(3)?,
                    rowid: row.get(4)?,
                },
            ))
        })
        .map_err(database_error)?;
    rows.collect::<rusqlite::Result<_>>()
        .map_err(database_error)
}

fn delete_document(transaction: &Transaction, rowid: i64) -> Result<()> {
    delete_search_data(transaction, rowid)?;
    transaction
        .execute("DELETE FROM documents WHERE rowid=?", [rowid])
        .map_err(database_error)?;
    Ok(())
}

fn delete_search_data(transaction: &Transaction, rowid: i64) -> Result<()> {
    for sql in [
        "DELETE FROM lexical WHERE rowid=?",
        "DELETE FROM semantic WHERE rowid=?",
    ] {
        transaction.execute(sql, [rowid]).map_err(database_error)?;
    }
    Ok(())
}

fn fingerprint(path: &Path) -> Result<Option<String>> {
    let Some(metadata) = io::info(path)? else {
        return Ok(None);
    };
    if !metadata.is_file() {
        return Err("Memory path is not a regular file".into());
    }
    #[cfg(unix)]
    let key = {
        use std::os::unix::fs::MetadataExt;
        format!(
            "{}:{}:{}:{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec()
        )
    };
    #[cfg(windows)]
    let key = {
        use std::os::windows::fs::MetadataExt;
        format!(
            "{}:{}:{}",
            metadata.file_size(),
            metadata.creation_time(),
            metadata.last_write_time()
        )
    };
    Ok(Some(key))
}

fn vector_blob(vector: &[f32]) -> Result<Vec<u8>> {
    if vector.len() != DIMENSIONS
        || vector.iter().any(|value| !value.is_finite())
        || vector.iter().all(|value| *value == 0.0)
    {
        return Err("Invalid memory embedding vector".into());
    }
    Ok(vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect())
}

fn field<'a>(record: &'a Value, key: &str) -> Result<&'a str> {
    record[key]
        .as_str()
        .ok_or_else(|| format!("Indexed memory entry is missing {key}"))
}

fn ranked(row: &rusqlite::Row) -> rusqlite::Result<Ranked> {
    Ok(Ranked {
        rowid: row.get(0)?,
        score: row.get(1)?,
    })
}

fn reciprocal_rank_fusion(lexical: Vec<Ranked>, semantic: Vec<Ranked>) -> Vec<Ranked> {
    let lexical_ranks: HashMap<_, _> = lexical
        .iter()
        .enumerate()
        .map(|(rank, candidate)| (candidate.rowid, rank))
        .collect();
    let mut totals = BTreeMap::<i64, f64>::new();
    for candidate_list in [lexical, semantic] {
        for (offset, candidate) in candidate_list.into_iter().enumerate() {
            *totals.entry(candidate.rowid).or_default() += 1.0 / (61.0 + offset as f64);
        }
    }
    let mut ranked = totals
        .into_iter()
        .map(|(rowid, score)| Ranked { rowid, score })
        .collect::<Vec<_>>();
    let lexical_rank = |rowid| lexical_ranks.get(&rowid).copied().unwrap_or(usize::MAX);
    ranked.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then(lexical_rank(left.rowid).cmp(&lexical_rank(right.rowid)))
            .then(left.rowid.cmp(&right.rowid))
    });
    ranked
}

fn validate(query: &str, limit: usize, kind: Option<&str>) -> Result<()> {
    if query.chars().count() > 500 || !(1..=50).contains(&limit) {
        return Err("Invalid memory search query or limit".into());
    }
    if kind.is_some_and(|kind| !matches!(kind, "decision" | "preference" | "correction")) {
        return Err("Invalid memory entry kind".into());
    }
    Ok(())
}

struct LexicalQuery {
    terms: BTreeSet<String>,
    stems: BTreeSet<String>,
    expression: Option<String>,
}

impl LexicalQuery {
    fn new(query: &str) -> Self {
        let folded = query.case_fold().collect::<String>();
        let mut terms = BTreeSet::new();
        for term in WORD.find_iter(&folded).map(|term| term.as_str()) {
            if !STOP_WORDS.contains(term) {
                terms.insert(term.to_owned());
                if terms.len() == 20 {
                    break;
                }
            }
        }
        let stems = terms
            .iter()
            .filter_map(|term| russian_stem(term))
            .collect::<BTreeSet<_>>();
        let expression = (!terms.is_empty()).then(|| {
            let mut expression = format!("{{title body}} : ({})", fts_alternatives(&terms));
            if !stems.is_empty() {
                expression.push_str(&format!(" OR forms : ({})", fts_alternatives(&stems)));
            }
            expression
        });
        Self {
            terms,
            stems,
            expression,
        }
    }

    fn snippet(&self, body: &str) -> String {
        const WIDTH: usize = 240;
        let matching_offset = WORD
            .find_iter(body)
            .find(|term| {
                let folded = term.as_str().case_fold().collect::<String>();
                self.terms.contains(&folded)
                    || russian_stem(&folded).is_some_and(|stem| self.stems.contains(&stem))
            })
            .map_or(0, |term| term.start());
        let start_chars = body[..matching_offset].chars().count().saturating_sub(64);
        let mut tail = body.char_indices().skip(start_chars);
        let start = tail.next().map_or(0, |(byte, _)| byte);
        let end = tail.nth(WIDTH - 1).map_or(body.len(), |(byte, _)| byte);
        let excerpt = body[start..end]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "{}{}{}",
            if start == 0 { "" } else { "…" },
            excerpt,
            if end == body.len() { "" } else { "…" }
        )
    }
}

fn russian_stem(word: &str) -> Option<String> {
    if !word.chars().all(|ch| matches!(ch, 'а'..='я' | 'ё')) {
        return None;
    }
    let normalized = word.replace('ё', "е");
    let stem = STEMMER.stem(&normalized);
    (stem.chars().count() >= 3).then(|| stem.into_owned())
}

fn fts_alternatives(words: &BTreeSet<String>) -> String {
    words
        .iter()
        .map(|word| format!("\"{word}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests;
