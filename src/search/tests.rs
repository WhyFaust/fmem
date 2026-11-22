use super::*;
use crate::memory::{format, shared};
use crate::test_directory as tempdir;
use serde_json::json;
use tempfile::TempDir;

fn fixture() -> (TempDir, Store, Binding) {
    let temporary = tempdir().unwrap();
    let vault = temporary.path().join("vault");
    let repo = temporary.path().join("repo");
    fs::create_dir_all(vault.join(".obsidian")).unwrap();
    fs::create_dir(&repo).unwrap();
    let store = Store::new(temporary.path().join("state")).unwrap();
    let binding = store.setup(&vault, &repo, "sample", Some("auto")).unwrap();
    (temporary, store, binding)
}

fn save(store: &Store, binding: &Binding, title: &str, body: &str, kind: &str) -> String {
    let value = store
        .save(
            binding,
            &json!({"kind":kind,"title":title,"body":body}),
            true,
            None,
            None,
        )
        .unwrap();
    value["id"].as_str().unwrap().into()
}

fn compact(store: &Store, binding: &Binding, query: &str) -> Vec<SearchHit> {
    store
        .search_compact(binding, query, 10, SearchMode::Lexical, None, None)
        .unwrap()
        .hits
}

#[test]
fn persistent_search_follows_external_edit_deletion_and_rebuild() {
    let (_temporary, store, binding) = fixture();
    let id = save(
        &store,
        &binding,
        "Проверка изменений",
        "Используй визуальные проверки скриншотов.",
        "decision",
    );
    let report = store.index(&binding, false, None).unwrap();
    assert_eq!((report.active, report.updated, report.pending), (1, 1, 1));
    let reopened = Store::new(&store.state).unwrap();
    let original = &compact(&reopened, &binding, "скриншотам")[0];
    assert_eq!(original.id, id);
    assert_eq!(
        original.source,
        format!("Codex Memory/Projects/sample/Records/{id}/Проверка изменений.md")
    );
    assert!(original.snippet.contains("скриншотов"));
    let report = reopened.index(&binding, false, None).unwrap();
    assert_eq!((report.active, report.updated, report.pending), (1, 0, 1));

    let root = store.root(&binding).unwrap();
    let original_path = store::record(&root, &id).unwrap();
    let path = original_path.with_file_name("Проверка интерфейса.md");
    fs::rename(&original_path, &path).unwrap();
    let renamed = &compact(&reopened, &binding, "скриншотам")[0];
    assert_eq!(renamed.id, id);
    assert_eq!(renamed.revision, original.revision);
    assert_eq!(
        renamed.source,
        format!("Codex Memory/Projects/sample/Records/{id}/Проверка интерфейса.md")
    );
    let note = fs::read_to_string(&path).unwrap();
    fs::write(&path, note.replace("скриншотов", "видеозаписей")).unwrap();
    assert!(compact(&store, &binding, "скриншотам").is_empty());
    let edited = &compact(&store, &binding, "видеозаписей")[0];
    assert_eq!(edited.id, id);
    assert_ne!(edited.revision, original.revision);
    assert_eq!(
        store.search(&binding, "видеозаписей", 1).unwrap()[0]["body"],
        "Используй визуальные проверки видеозаписей."
    );

    let index_dir = store.state.join("indexes");
    let database = fs::read_dir(index_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(&database, b"broken cache").unwrap();
    assert!(store.search(&binding, "видеозаписей", 1).is_err());
    assert_eq!(store.index(&binding, true, None).unwrap().updated, 1);
    assert_eq!(
        compact(&store, &binding, "видеозаписей")[0].revision,
        edited.revision
    );

    fs::remove_file(&path).unwrap();
    assert!(compact(&store, &binding, "видеозаписей").is_empty());
    assert_eq!(store.index(&binding, false, None).unwrap().active, 0);
}

#[test]
fn missing_cached_approval_rechecks_record_header() {
    let (_temporary, store, binding) = fixture();
    let predecessor = save(
        &store,
        &binding,
        "Original process",
        "Use the original narwhal protocol.",
        "decision",
    );
    let successor = store
        .save(
            &binding,
            &json!({
                "kind":"correction", "title":"Current process",
                "body":"Use the current narwhal protocol.", "supersedes":predecessor
            }),
            true,
            None,
            None,
        )
        .unwrap();
    let id = successor["id"].as_str().unwrap();
    assert_eq!(compact(&store, &binding, "narwhal")[0].id, id);
    let root = store.root(&binding).unwrap();
    let path = store::record(&root, id).unwrap();
    let raw = fs::read_to_string(&path).unwrap();
    fs::write(&path, raw.replace(&predecessor, "invalid-predecessor")).unwrap();
    let index =
        SearchIndex::open(&store.state, &root, binding.root.parent().unwrap(), false).unwrap();
    assert_eq!(
        index
            .connection
            .execute(
                "UPDATE headers SET approval='null', stamp=? WHERE id=?",
                params![fingerprint(&path).unwrap().unwrap(), id],
            )
            .unwrap(),
        1
    );

    assert!(store.search(&binding, "narwhal", 5).is_err());
    assert!(store.recall(&binding, "narwhal", 5).is_err());
}

#[test]
fn rebuild_recovers_missing_index_tables_without_changing_notes_or_receipts() {
    let (_temporary, store, binding) = fixture();
    let request = json!({"operations":[{
        "operation_id":"rebuild-preserves-receipt", "action":"remember",
        "candidate":{"kind":"decision","title":"Independent narwhal","body":"Keep the narwhal protocol."}
    }]});
    let committed = crate::operations::commit(&store, &binding, &request).unwrap();
    let receipt = committed["receipts"][0].clone();
    assert_eq!(receipt["status"], "persisted");
    let id = receipt["record_id"].as_str().unwrap();
    let note = store::record(&store.root(&binding).unwrap(), id).unwrap();
    let original_note = fs::read(&note).unwrap();
    let journal = store.state.join("operations.sqlite3");
    let original_journal = fs::read(&journal).unwrap();
    assert_eq!(compact(&store, &binding, "narwhal")[0].id, id);
    let database = fs::read_dir(store.state.join("indexes"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let open = Connection::open(&database).unwrap();
    open.execute_batch("DROP TABLE documents;").unwrap();
    assert!(store.search(&binding, "narwhal", 5).is_err());

    let report = store.index(&binding, true, None).unwrap();

    assert_eq!((report.active, report.updated, report.pending), (1, 1, 1));
    let hits = compact(&store, &binding, "narwhal");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, id);
    assert_eq!(
        open.query_row("SELECT id FROM documents", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        id
    );
    assert_eq!(fs::read(&note).unwrap(), original_note);
    assert_eq!(fs::read(&journal).unwrap(), original_journal);
    assert_eq!(
        crate::operations::receipt(&store, &binding, "rebuild-preserves-receipt").unwrap(),
        Some(receipt)
    );
}

#[test]
fn rebuild_refreshes_an_open_index_connection() {
    let (_temporary, store, binding) = fixture();
    let id = save(
        &store,
        &binding,
        "Procedure",
        "Old narwhal protocol.",
        "decision",
    );
    store.index(&binding, false, None).unwrap();
    let root = store.root(&binding).unwrap();
    let open =
        SearchIndex::open(&store.state, &root, binding.root.parent().unwrap(), false).unwrap();
    assert_eq!(
        open.lexical(&LexicalQuery::new("narwhal"), 5, None)
            .unwrap()
            .len(),
        1
    );
    let path = store::record(&root, &id).unwrap();
    let raw = fs::read_to_string(&path).unwrap();
    fs::write(&path, raw.replace("narwhal", "tortoise")).unwrap();

    assert_eq!(store.index(&binding, true, None).unwrap().active, 1);

    assert!(open
        .lexical(&LexicalQuery::new("narwhal"), 5, None)
        .unwrap()
        .is_empty());
    let updated = open
        .lexical(&LexicalQuery::new("tortoise"), 5, None)
        .unwrap();
    assert_eq!(updated.len(), 1);
    assert_eq!(open.document(updated[0].rowid).unwrap().1["id"], id);
}

#[test]
fn search_filters_scope_and_kind_before_top_k_and_lists_active_preferences() {
    let (_temporary, store, binding) = fixture();
    for i in 0..7 {
        save(
            &store,
            &binding,
            &format!("Hedgehog {i}"),
            "hedgehog hedgehog hedgehog",
            "decision",
        );
    }
    let preference = save(
        &store,
        &binding,
        "Garden",
        "Garden hedgehog preference",
        "preference",
    );
    let shared_record = shared::remember(
        &store,
        &binding,
        &json!({
            "kind":"preference","title":"Shared hedgehog","body":"Hedgehog shared note."
        }),
    )
    .unwrap();
    let shared_binding = shared::for_recall(&store, &binding).unwrap().unwrap();
    let filtered = store
        .search_compact(
            &binding,
            "hedgehog",
            1,
            SearchMode::Lexical,
            Some("preference"),
            None,
        )
        .unwrap();
    assert_eq!(filtered.hits[0].id, preference);
    assert_eq!(filtered.hits[0].scope, "sample");
    let recent = store
        .search_compact(
            &binding,
            "",
            3,
            SearchMode::Lexical,
            Some("preference"),
            None,
        )
        .unwrap();
    assert_eq!(recent.hits.len(), 1);
    assert_eq!(recent.hits[0].id, preference);
    let shared_hits = compact(&store, &shared_binding, "hedgehog");
    assert_eq!(shared_hits.len(), 1);
    assert_eq!(shared_hits[0].id, shared_record["id"]);
    assert_eq!(shared_hits[0].scope, "shared");

    let fallback = store
        .search_compact(
            &binding,
            "hedgehog",
            1,
            SearchMode::Hybrid,
            Some("preference"),
            None,
        )
        .unwrap();
    assert_eq!(fallback.hits[0].id, preference);
    assert!(fallback.semantic_unavailable.is_some());
    let unavailable = store
        .search_compact(&binding, "hedgehog", 1, SearchMode::Semantic, None, None)
        .unwrap();
    assert!(unavailable.hits.is_empty());
    assert!(unavailable.semantic_unavailable.is_some());

    let root = store.root(&binding).unwrap();
    let mut index =
        SearchIndex::open(&store.state, &root, binding.root.parent().unwrap(), false).unwrap();
    let mut vector = vec![0.0; DIMENSIONS];
    vector[0] = 1.0;
    let candidates = index.pending(0, 32).unwrap();
    let vectors = candidates
        .into_iter()
        .map(|candidate| {
            let mut note_vector = vector.clone();
            if candidate.text.contains("Garden") {
                note_vector[1] = 1.0;
            }
            (candidate.rowid, candidate.revision, note_vector)
        })
        .collect::<Vec<_>>();
    assert_eq!(index.commit_vectors(vectors).unwrap(), 8);
    let unfiltered = index.semantic(&vector, 1, None).unwrap();
    assert_ne!(
        index.document(unfiltered[0].rowid).unwrap().1["id"],
        preference
    );
    let filtered = index.semantic(&vector, 1, Some("preference")).unwrap();
    assert_eq!(
        index.document(filtered[0].rowid).unwrap().1["id"],
        preference
    );
}

#[test]
fn late_embeddings_cannot_restore_edited_superseded_or_forgotten_records() {
    let (_temporary, store, binding) = fixture();
    let obsolete = save(
        &store,
        &binding,
        "Obsolete quasar",
        "Retire the quasar.",
        "decision",
    );
    let editable = save(
        &store,
        &binding,
        "Editable fixture",
        "The old arrangement.",
        "decision",
    );
    let forgotten = save(
        &store,
        &binding,
        "Forgotten tornado",
        "Old tornado procedure.",
        "decision",
    );
    let root = store.root(&binding).unwrap();
    let mut index =
        SearchIndex::open(&store.state, &root, binding.root.parent().unwrap(), false).unwrap();
    index
        .refresh(&store, &root, Refresh::Incremental, false)
        .unwrap();
    let pending = index.pending(0, 32).unwrap();
    assert_eq!(pending.len(), 3);

    let successor = store.save(&binding, &json!({
        "kind":"decision", "title":"Current process", "body":"Adopt the current process.", "supersedes":obsolete
    }), true, None, None).unwrap();
    let path = store::record(&root, &editable).unwrap();
    fs::write(
        &path,
        fs::read_to_string(&path)
            .unwrap()
            .replace("old arrangement", "new arrangement"),
    )
    .unwrap();
    store.forget(&binding, &forgotten).unwrap();
    let report = index
        .refresh(&store, &root, Refresh::Incremental, false)
        .unwrap();
    assert_eq!((report.active, report.updated, report.removed), (2, 2, 2));
    let mut vector = vec![0.0; DIMENSIONS];
    vector[0] = 1.0;
    let late = pending
        .into_iter()
        .map(|note| (note.rowid, note.revision, vector.clone()))
        .collect();
    assert_eq!(index.commit_vectors(late).unwrap(), 0);
    assert!(index.semantic(&vector, 10, None).unwrap().is_empty());
    let active = index.pending(0, 32).unwrap();
    assert_eq!(active.len(), 2);
    assert_eq!(
        index
            .commit_vectors(
                active
                    .into_iter()
                    .map(|note| (note.rowid, note.revision, vector.clone()))
                    .collect()
            )
            .unwrap(),
        2
    );
    assert_eq!(index.semantic(&vector, 10, None).unwrap().len(), 2);

    store
        .forget(&binding, successor["id"].as_str().unwrap())
        .unwrap();
    index
        .refresh(&store, &root, Refresh::Incremental, false)
        .unwrap();
    assert_eq!(index.semantic(&vector, 10, None).unwrap().len(), 1);
    assert!(compact(&store, &binding, "quasar tornado").is_empty());
}

#[test]
fn reciprocal_rank_fusion_rewards_agreement_and_retains_both_channels() {
    let first = vec![
        Ranked {
            rowid: 1,
            score: 9.0,
        },
        Ranked {
            rowid: 2,
            score: 4.0,
        },
    ];
    let second = vec![
        Ranked {
            rowid: 3,
            score: 0.91,
        },
        Ranked {
            rowid: 2,
            score: 0.8,
        },
    ];
    let merged = reciprocal_rank_fusion(first, second);
    assert_eq!(
        merged.iter().map(|row| row.rowid).collect::<Vec<_>>(),
        [2, 1, 3]
    );
    let lexical = vec![Ranked {
        rowid: 3,
        score: 0.1,
    }];
    let semantic = vec![Ranked {
        rowid: 1,
        score: 0.9,
    }];
    assert_eq!(reciprocal_rank_fusion(lexical, semantic)[0].rowid, 3);
}

#[test]
#[cfg(feature = "local-embeddings")]
#[ignore = "requires a verified local model installation and ONNX Runtime"]
fn multilingual_model_recovers_russian_and_english_paraphrases() {
    let (_temporary, store, binding) = fixture();
    let model_state = if let Some(path) = std::env::var_os("FMEM_TEST_MODEL_STATE") {
        std::path::PathBuf::from(path)
    } else {
        let source = std::env::var_os("FMEM_TEST_MODEL_SOURCE")
            .expect("set FMEM_TEST_MODEL_STATE or FMEM_TEST_MODEL_SOURCE");
        Embeddings::install(&store.state, Path::new(&source)).unwrap();
        store.state.clone()
    };
    let mut embeddings = Embeddings::load(&model_state).unwrap();
    let rus = save(
        &store,
        &binding,
        "Порядок доставки",
        "Отправляй посылки покупателям через почтовое отделение.",
        "decision",
    );
    let eng = save(
        &store,
        &binding,
        "Vacation request",
        "Submit annual leave through the human resources portal.",
        "decision",
    );
    save(
        &store,
        &binding,
        "Whale watching",
        "Hydrophones measure underwater vocalizations from marine mammals.",
        "decision",
    );
    save(
        &store,
        &binding,
        "Обслуживание",
        "Очисти фильтр кондиционера и проверь компрессор.",
        "decision",
    );

    let report = store.index(&binding, false, Some(&mut embeddings)).unwrap();
    assert_eq!((report.active, report.pending, report.embedded), (4, 0, 4));
    for (query, target) in [
        ("Как переслать заказ клиенту?", rus.as_str()),
        ("Where do I book time off work?", eng.as_str()),
    ] {
        assert!(compact(&store, &binding, query).is_empty());
        for mode in [SearchMode::Semantic, SearchMode::Hybrid] {
            let result = store
                .search_compact(&binding, query, 1, mode, None, Some(&mut embeddings))
                .unwrap();
            assert!(result.semantic_unavailable.is_none());
            assert_eq!(result.hits[0].id, target, "{query}: {:?}", result.hits);
            println!(
                "{mode:?} {query} -> {} ({:.3})",
                result.hits[0].title, result.hits[0].score
            );
        }
    }
    let report = store.index(&binding, false, Some(&mut embeddings)).unwrap();
    assert_eq!((report.updated, report.pending, report.embedded), (0, 0, 0));
    let latest = save(
        &store,
        &binding,
        "Solar telescope",
        "Observe sunspots at dawn.",
        "decision",
    );
    let partial = store
        .search_compact(
            &binding,
            "solar telescope",
            1,
            SearchMode::Hybrid,
            None,
            Some(&mut embeddings),
        )
        .unwrap();
    assert_eq!(partial.hits[0].id, latest);
    assert!(partial
        .semantic_unavailable
        .unwrap()
        .contains("1 pending in scope"));
    let blocked = store
        .update_index_batch(&binding, &mut embeddings, 1, || false)
        .unwrap_err();
    assert_eq!(blocked, "Memory binding changed during embedding");
    let report = store.index(&binding, false, None).unwrap();
    assert_eq!((report.pending, report.embedded), (1, 0));
    let report = store
        .update_index_batch(&binding, &mut embeddings, 1, || true)
        .unwrap();
    assert_eq!((report.pending, report.embedded), (0, 1));
}

#[test]
#[ignore = "compares cold/warm lookup with the legacy in-memory rebuild"]
fn benchmark_persistent_against_legacy() {
    use std::time::Instant;
    let (_temporary, store, binding) = fixture();
    let root = store.root(&binding).unwrap();
    for n in 0..750 {
        let id = io::hash(n.to_string());
        let (title, body) = match n % 3 {
            0 => (
                "Pagination tests",
                "Exercise page boundaries, continuation tokens and empty response fixtures.",
            ),
            1 => (
                "Доставка заказа",
                "Проверь адрес получателя перед отправкой посылок и выставлением счета.",
            ),
            _ => (
                "Migration procedure",
                "Restore a clean staging snapshot before applying schema changes.",
            ),
        };
        let fields = format::Fields::from([
            ("id".into(), id.clone()),
            ("title".into(), format!("{title} {n}")),
            ("kind".into(), "decision".into()),
            ("created".into(), "2026-01-01T00:00:00+00:00".into()),
            ("fstack_status".into(), "approved".into()),
        ]);
        let directory = root.join("Records").join(&id);
        fs::create_dir(&directory).unwrap();
        fs::write(
            directory.join(format!("{title} {n}.md")),
            format::markdown(format::MEMORY, &fields, body),
        )
        .unwrap();
    }
    for query in [
        "pagination boundaries",
        "отправка посылки",
        "schema migration",
    ] {
        let mut legacy_times = Vec::new();
        let mut warm_times = Vec::new();
        fs::remove_dir_all(store.state.join("indexes")).ok();
        let start = Instant::now();
        let cold = store.search(&binding, query, 10).unwrap();
        let cold_time = start.elapsed();
        let ids = |records: &Value| {
            records
                .as_array()
                .unwrap()
                .iter()
                .map(|record| record["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        for _ in 0..9 {
            let start = Instant::now();
            let legacy = legacy_ids(&store, &root, query);
            legacy_times.push(start.elapsed());
            let start = Instant::now();
            let warm = store.search(&binding, query, 10).unwrap();
            warm_times.push(start.elapsed());
            assert_eq!(ids(&cold), legacy);
            assert_eq!(ids(&warm), legacy);
        }
        legacy_times.sort_unstable();
        warm_times.sort_unstable();
        println!("{query:?}, 750 records, 9 samples, top 10 identical: legacy p50 {:?} [{:?}, {:?}], persistent cold {:?}, warm p50 {:?} [{:?}, {:?}]",
            legacy_times[4], legacy_times[0], legacy_times[8], cold_time, warm_times[4], warm_times[0], warm_times[8]);
    }
}

fn legacy_ids(store: &Store, root: &Path, query: &str) -> Vec<String> {
    let query = LexicalQuery::new(query);
    let expression = query.expression.unwrap();
    let (approvals, hidden) = store.statuses(root).unwrap();
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch("CREATE VIRTUAL TABLE entries USING fts5(id UNINDEXED,title,body,forms,created UNINDEXED)").unwrap();
    for (id, approval) in approvals {
        if hidden.contains(&id) {
            continue;
        }
        let record = store
            .search_record(root, &id, &approval, true)
            .unwrap()
            .unwrap();
        let title = record["title"]
            .as_str()
            .unwrap()
            .case_fold()
            .collect::<String>();
        let body = record["body"]
            .as_str()
            .unwrap()
            .case_fold()
            .collect::<String>();
        let forms = if query.stems.is_empty() {
            String::new()
        } else {
            WORD.find_iter(&title)
                .chain(WORD.find_iter(&body))
                .filter_map(|m| russian_stem(m.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        };
        connection
            .execute(
                "INSERT INTO entries VALUES (?,?,?,?,?)",
                params![id, title, body, forms, record["created"].as_str()],
            )
            .unwrap();
    }
    let mut select = connection.prepare(
        "SELECT id FROM entries WHERE entries MATCH ? ORDER BY bm25(entries,0,2,1,0.35,0), julianday(created) DESC, id LIMIT 10"
    ).unwrap();
    select
        .query_map([expression], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}
