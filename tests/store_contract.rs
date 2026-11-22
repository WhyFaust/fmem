use fmem::{
    memory::{format, io, scope, store, views, Binding, Store},
    protocol::{action, decode, encode, read_json},
};
use serde_json::{json, Value};
#[cfg(windows)]
use std::path::Path;
use std::{collections::BTreeMap, fs, path::PathBuf};
struct Fixture {
    _temp: tempfile::TempDir,
    base: PathBuf,
    repo: PathBuf,
    vault: PathBuf,
    store: Store,
    binding: Binding,
}
impl Fixture {
    fn new(mode: &str, slug: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        let vault = root.join("vault");
        fs::create_dir(&repo).unwrap();
        fs::create_dir_all(vault.join(".obsidian")).unwrap();
        let store = Store::new(root.join("state")).unwrap();
        let binding = store.setup(&vault, &repo, slug, Some(mode)).unwrap();
        Self {
            _temp: temp,
            base: root,
            repo,
            vault,
            store,
            binding,
        }
    }
    fn root(&self) -> PathBuf {
        self.store.root(&self.binding).unwrap()
    }
    fn save(&self, body: &str, approved: bool) -> Value {
        self.store
            .save(&self.binding, &candidate(body), approved, None, None)
            .unwrap()
    }
    fn marker(&self, folder: &str, id: &str, predecessor: Option<&str>) -> PathBuf {
        let p = self.root().join(folder).join(format!("{id}.md"));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = BTreeMap::from([("id".into(), id.into())]);
        if let Some(s) = predecessor {
            f.insert("supersedes".into(), s.into());
        }
        fs::write(
            &p,
            format::markdown(
                if folder == "Approved" {
                    format::APPROVED
                } else {
                    format::FORGOTTEN
                },
                &f,
                "",
            ),
        )
        .unwrap();
        p
    }
}
fn candidate(body: &str) -> Value {
    json!({"kind":"decision","title":"Heliotrope convention","body":body})
}
fn id(v: &Value) -> &str {
    v["id"].as_str().unwrap()
}

fn replace_property(raw: &str, property: &str, value: &str) -> String {
    let prefix = format!("{property}: ");
    let original = raw.lines().find(|line| line.starts_with(&prefix)).unwrap();
    raw.replacen(original, &format!("{prefix}{}", json!(value)), 1)
}

#[test]
fn validation_and_stable_identity() {
    let c = json!({"kind":"decision","title":"Порог ожидания","body":"Использовать три секунды для чтения конфигурации."});
    let fields = format::candidate(&c).unwrap();
    assert_eq!(
        format::stable_id(&fields),
        "8acdbb7d4e00bec1161538761006b168f1cf58c7244bf0f0dcecb7ca0f98a7b8"
    );
    let mut sourced = fields.clone();
    sourced.insert("source".into(), "another source".into());
    assert_eq!(format::stable_id(&fields), format::stable_id(&sourced));
    for text in [
        "token=secret123",
        "sk-1234567890123456",
        "![image](url)",
        "<script>x</script>",
        "```python\nx\n```",
        "x\u{0080}",
    ] {
        assert!(format::candidate(&candidate(text)).is_err(), "{text}");
    }
    for slug in ["con", "COM1", "com1", "../escape", "a/b", ""] {
        assert!(format::project(slug).is_err());
    }
    assert!(format::project("token-authority").is_ok());
    for raw in [
        r#"{"action":"skip","action":"skip"}"#,
        r#"{"x":{"a":1,"a":2}}"#,
        r#"[]"#,
    ] {
        assert!(read_json(raw.as_bytes(), 20000).is_err());
    }
    assert!(read_json(&b"{} "[..], 2).is_err());
    for a in [
        json!({"action":"skip"}),
        json!({"action":"remember","candidate":c}),
        json!({"action":"forget","id":"a".repeat(64)}),
    ] {
        let canonical = action(&a).unwrap();
        assert_eq!(decode(&encode(&canonical).unwrap()).unwrap(), canonical);
    }
    assert!(action(&json!({"action":"skip","reason":"none"})).is_err());
    assert!(decode("e30=").is_err());
}

#[test]
fn record_filename_is_readable_and_identity_is_stable() {
    let f = Fixture::new("auto", "pilot");
    let value = json!({"kind":"decision","title":"Порог ожидания","body":"Проверять локальную конфигурацию перед запуском."});
    let saved = f.store.save(&f.binding, &value, true, None, None).unwrap();
    let path = store::record(&f.root(), id(&saved)).unwrap();
    assert_eq!(path.file_stem().unwrap(), "Порог ожидания");
    assert_eq!(path.parent().unwrap().file_name().unwrap(), id(&saved));
    assert_eq!(
        f.store.save(&f.binding, &value, true, None, None).unwrap(),
        saved
    );
    let another = f.store.save(&f.binding, &json!({
        "kind":"decision","title":"Порог ожидания","body":"Проверять доступность хранилища перед записью."
    }), true, None, None).unwrap();
    let another_path = store::record(&f.root(), id(&another)).unwrap();
    assert_ne!(id(&saved), id(&another));
    assert_eq!(path.file_name(), another_path.file_name());
    assert_ne!(path.parent(), another_path.parent());
    assert_eq!(f.store.get(&f.binding, id(&saved)).unwrap(), saved);
    assert_eq!(f.store.get(&f.binding, id(&another)).unwrap(), another);
}

#[test]
fn record_filenames_preserve_unicode_and_are_portable() {
    let f = Fixture::new("auto", "pilot");
    for (title, basename) in [
        ("CON", "Запись CON"),
        ("aux.txt", "Запись aux.txt"),
        ("COM¹", "Запись COM¹"),
        ("LPT9", "Запись LPT9"),
        ("..", "Запись"),
        (
            "Что: где/когда? #метка | выбор",
            "Что где когда метка выбор",
        ),
        ("Read [config] \\ once", "Read config once"),
        ("Проверка 👩‍💻", "Проверка 👩‍💻"),
    ] {
        let saved = f
            .store
            .save(
                &f.binding,
                &json!({
                    "kind":"decision","title":title,"body":"Проверить сохранённое правило."
                }),
                true,
                None,
                None,
            )
            .unwrap();
        let path = store::record(&f.root(), id(&saved)).unwrap();
        assert_eq!(path.file_stem().unwrap(), basename);
        assert_eq!(saved["title"], title);
        assert_eq!(f.store.get(&f.binding, id(&saved)).unwrap(), saved);
    }
    let title = "Очень длинный заголовок ".repeat(6);
    let saved = f.store.save(&f.binding, &json!({
        "kind":"decision","title":title.trim(),"body":"Полный заголовок остаётся в метаданных."
    }), true, None, None).unwrap();
    let path = store::record(&f.root(), id(&saved)).unwrap();
    let basename = path.file_stem().unwrap().to_str().unwrap();
    assert!(basename.len() <= 120);
    assert!(basename.chars().count() <= 64);
    assert!(!basename.ends_with([' ', '.']));
    assert_eq!(saved["title"], title.trim());
}

#[test]
fn renaming_a_note_preserves_reads_history_and_idempotent_save() {
    let f = Fixture::new("auto", "pilot");
    let saved = f.save("Original heliotrope", true);
    let original = store::record(&f.root(), id(&saved)).unwrap();
    let renamed = original.with_file_name("Уточнённое правило.md");
    fs::rename(&original, &renamed).unwrap();
    assert_eq!(store::record(&f.root(), id(&saved)).unwrap(), renamed);
    assert_eq!(f.store.get(&f.binding, id(&saved)).unwrap(), saved);
    assert_eq!(f.save("Original heliotrope", true), saved);
    assert_eq!(
        f.store.history(&f.binding, id(&saved), 5).unwrap()["entries"],
        json!([saved])
    );
    assert_eq!(store::records(&f.root()).unwrap(), vec![renamed.clone()]);
    f.store.forget(&f.binding, id(&saved)).unwrap();
    assert!(!renamed.exists());
    assert!(store::record(&f.root(), id(&saved)).is_err());
}

#[test]
fn forgetting_removes_empty_id_directories_and_preserves_other_content() {
    for keep_other_content in [false, true] {
        let f = Fixture::new("auto", "pilot");
        let saved = f.save("Original heliotrope", true);
        let note = store::record(&f.root(), id(&saved)).unwrap();
        let directory = note.parent().unwrap();
        if keep_other_content {
            fs::write(directory.join("Personal.txt"), "Keep this file").unwrap();
            fs::create_dir(directory.join("Attachments")).unwrap();
            fs::write(
                directory.join("Attachments/Personal.txt"),
                "Keep this attachment",
            )
            .unwrap();
        }
        for _ in 0..2 {
            assert_eq!(
                f.store.forget(&f.binding, id(&saved)).unwrap()["status"],
                "forgotten"
            );
            assert!(!note.exists());
            assert!(store::tombstone_ids(&f.root())
                .unwrap()
                .contains(id(&saved)));
            if keep_other_content {
                assert_eq!(
                    fs::read_to_string(directory.join("Personal.txt")).unwrap(),
                    "Keep this file"
                );
                assert_eq!(
                    fs::read_to_string(directory.join("Attachments/Personal.txt")).unwrap(),
                    "Keep this attachment"
                );
            } else {
                assert!(!directory.exists());
            }
        }
    }
}

#[test]
fn ambiguous_record_directories_fail_closed_without_changing_either_note() {
    let f = Fixture::new("auto", "pilot");
    let saved = f.save("Original heliotrope", true);
    assert_eq!(
        f.store.search(&f.binding, "heliotrope", 5).unwrap(),
        json!([saved])
    );
    let path = store::record(&f.root(), id(&saved)).unwrap();
    let duplicate = path.with_file_name("Другой вариант.md");
    let raw = fs::read(&path).unwrap();
    fs::write(&duplicate, &raw).unwrap();
    assert!(store::record(&f.root(), id(&saved))
        .unwrap_err()
        .contains("multiple Markdown"));
    assert!(f.store.get(&f.binding, id(&saved)).is_err());
    assert!(f.store.search(&f.binding, "heliotrope", 5).is_err());
    assert!(f.store.inbox(&f.binding).is_err());
    assert!(f.store.approve(&f.binding, id(&saved)).is_err());
    assert!(f.store.forget(&f.binding, id(&saved)).is_err());
    assert!(store::tombstone_ids(&f.root()).unwrap().is_empty());
    assert_eq!(fs::read(path).unwrap(), raw);
    assert_eq!(fs::read(duplicate).unwrap(), raw);
}

#[test]
fn record_identity_must_match_its_directory_and_graph_links_resolve_readable_paths() {
    let f = Fixture::new("auto", "pilot");
    let first = f.save("Original heliotrope", true);
    let old = store::record(&f.root(), id(&first)).unwrap();
    let renamed = old.with_file_name("Исходное правило.md");
    fs::rename(&old, &renamed).unwrap();
    let second = f.store.save(&f.binding, &json!({
        "kind":"correction","title":"Исправленное правило","body":"Use revised heliotrope.","supersedes":id(&first)
    }), true, None, None).unwrap();
    let path = store::record(&f.root(), id(&second)).unwrap();
    let raw = fs::read_to_string(&path).unwrap();
    let parsed = format::parse(&raw, id(&second), format::MEMORY, true).unwrap();
    let link = format!(
        "[[Codex Memory/Projects/pilot/Records/{}/Исходное правило.md]]",
        id(&first)
    );
    assert_eq!(parsed["supersedes_link"], link);
    assert_eq!(parsed["supersedes"], id(&first));
    let mismatch = raw.replace(
        &format!("id: \"{}\"", id(&second)),
        &format!("id: \"{}\"", id(&first)),
    );
    fs::write(&path, mismatch).unwrap();
    assert!(f.store.get(&f.binding, id(&second)).is_err());
    fs::write(&path, raw).unwrap();
    f.store.forget(&f.binding, id(&first)).unwrap();
    f.store.approve(&f.binding, id(&second)).unwrap();
    assert!(!fs::read_to_string(&path)
        .unwrap()
        .contains("supersedes_link"));
    assert_eq!(
        f.store.history(&f.binding, id(&second), 5).unwrap()["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn obsidian_link_rewrites_do_not_change_identity_history_or_search() {
    let f = Fixture::new("auto", "pilot");
    let first = f.save("Original heliotrope", true);
    let second = f.store.save(&f.binding, &json!({
        "kind":"correction","title":"Current Neptune","body":"Use revised aquamarine.","supersedes":id(&first)
    }), true, None, None).unwrap();
    let prior_history = f.store.history(&f.binding, id(&second), 5).unwrap();
    assert_eq!(
        f.store.search(&f.binding, "heliotrope", 5).unwrap(),
        json!([])
    );
    assert_eq!(
        f.store.search(&f.binding, "aquamarine", 5).unwrap(),
        json!([second])
    );
    let predecessor = store::record(&f.root(), id(&first)).unwrap();
    fs::rename(
        &predecessor,
        predecessor.with_file_name("Renamed source note.md"),
    )
    .unwrap();
    let path = store::record(&f.root(), id(&second)).unwrap();
    let original = fs::read_to_string(&path).unwrap();
    for rewritten in [
        "[[Renamed source note]]".to_owned(),
        format!(
            "[[Codex Memory/Projects/pilot/Records/{}/Renamed source note]]",
            id(&first)
        ),
        format!("[[../{}/Renamed source note.md]]", id(&first)),
        "[[Renamed source note|Original name]]".to_owned(),
        "[[Renamed source note#Details|Original: rule]]".to_owned(),
        "[[Codex Memory/Projects/another-project/Other note]]".to_owned(),
    ] {
        let changed = replace_property(&original, "supersedes_link", &rewritten);
        let changed = replace_property(&changed, "project_link", "[[pilot|Project notes]]");
        let changed = changed
            .replace(&json!(&rewritten).to_string(), &rewritten)
            .replace('\n', "\r\n");
        fs::write(&path, changed).unwrap();
        assert_eq!(f.store.get(&f.binding, id(&second)).unwrap(), second);
        assert!(f.store.get(&f.binding, id(&first)).is_err());
        assert_eq!(
            f.store.history(&f.binding, id(&second), 5).unwrap(),
            prior_history
        );
        assert_eq!(
            f.store.search(&f.binding, "heliotrope", 5).unwrap(),
            json!([])
        );
        assert_eq!(
            f.store.search(&f.binding, "aquamarine", 5).unwrap(),
            json!([second])
        );
    }
}

#[test]
fn graph_properties_reject_external_embedded_malformed_or_unbounded_links() {
    let f = Fixture::new("auto", "pilot");
    let first = f.save("Original heliotrope", true);
    let second = f.store.save(&f.binding, &json!({
        "kind":"correction","title":"Current Neptune","body":"Use revised aquamarine.","supersedes":id(&first)
    }), true, None, None).unwrap();
    let original = fs::read_to_string(store::record(&f.root(), id(&second)).unwrap()).unwrap();
    for property in ["project_link", "supersedes_link"] {
        for invalid in [
            "https://example.com/note".to_owned(),
            "[[https://example.com/note]]".to_owned(),
            "[[//example.com/note]]".to_owned(),
            "![[Note]]".to_owned(),
            "[[Note]][[Other]]".to_owned(),
            "[[Note]".to_owned(),
            "[[Note]] extra".to_owned(),
            "[[]]".to_owned(),
            "[[Note|]]".to_owned(),
            "[[Note|Alias|Other]]".to_owned(),
            "[[Note\nOther]]".to_owned(),
            "[[Note\u{0085}]]".to_owned(),
            "[[<script>]]".to_owned(),
            "[[\\\\server\\note]]".to_owned(),
            format!("[[{}]]", "a".repeat(513)),
        ] {
            let changed = replace_property(&original, property, &invalid);
            assert!(
                format::parse(&changed, id(&second), format::MEMORY, true).is_err(),
                "{property}: {invalid}"
            );
        }
        let duplicate = original.replacen("---\n", &format!("---\n{property}: \"[[Note]]\"\n"), 1);
        assert!(format::parse(&duplicate, id(&second), format::MEMORY, true).is_err());
    }
}

#[test]
fn dotted_titles_use_the_full_markdown_filename_in_predecessor_links() {
    let f = Fixture::new("auto", "pilot");
    for title in ["AGENTS.md", "archive.tar.gz", "API.v2"] {
        fs::write(f.vault.join(title), "Unrelated vault file").unwrap();
        let first = f
            .store
            .save(
                &f.binding,
                &json!({
                    "kind":"decision","title":title,"body":"Read the documented project rule."
                }),
                true,
                None,
                None,
            )
            .unwrap();
        let second = f.store.save(&f.binding, &json!({
            "kind":"correction","title":"Revised rule","body":"Use the revised project rule.","supersedes":id(&first)
        }), true, None, None).unwrap();
        let predecessor = store::record(&f.root(), id(&first)).unwrap();
        assert_eq!(
            predecessor.file_name().unwrap().to_str().unwrap(),
            format!("{title}.md")
        );
        let raw = fs::read_to_string(store::record(&f.root(), id(&second)).unwrap()).unwrap();
        let fields = format::parse(&raw, id(&second), format::MEMORY, true).unwrap();
        let relative = predecessor
            .strip_prefix(&f.vault)
            .unwrap()
            .iter()
            .map(|part| part.to_str().unwrap())
            .collect::<Vec<_>>()
            .join("/");
        assert_eq!(fields["supersedes_link"], format!("[[{relative}]]"));
        assert!(f.vault.join(relative).is_file());
        assert_eq!(
            f.store.history(&f.binding, id(&second), 5).unwrap()["entries"],
            json!([second, first])
        );
    }
}

#[cfg(unix)]
#[test]
fn record_resolution_rejects_symlinked_notes_and_id_directories() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new("auto", "pilot");
    let saved = f.save("Original heliotrope", true);
    let path = store::record(&f.root(), id(&saved)).unwrap();
    let raw = fs::read(&path).unwrap();
    let outside = f.base.join("outside.md");
    fs::rename(&path, &outside).unwrap();
    symlink(&outside, &path).unwrap();
    assert!(store::record(&f.root(), id(&saved)).is_err());
    assert!(f.store.get(&f.binding, id(&saved)).is_err());
    assert!(f.store.forget(&f.binding, id(&saved)).is_err());
    assert_eq!(fs::read(&outside).unwrap(), raw);
    fs::remove_file(&path).unwrap();
    fs::rename(&outside, &path).unwrap();
    let external_directory = f.base.join("outside-directory");
    let directory = path.parent().unwrap();
    fs::rename(directory, &external_directory).unwrap();
    symlink(&external_directory, directory).unwrap();
    assert!(store::record(&f.root(), id(&saved)).is_err());
    assert!(f.store.get(&f.binding, id(&saved)).is_err());
    assert!(f.store.forget(&f.binding, id(&saved)).is_err());
    assert!(store::tombstone_ids(&f.root()).unwrap().is_empty());
    assert_eq!(
        fs::read(external_directory.join(path.file_name().unwrap())).unwrap(),
        raw
    );
}

#[cfg(windows)]
#[test]
fn record_resolution_rejects_junctions_in_id_directories() {
    let f = Fixture::new("auto", "pilot");
    let saved = f.save("Original heliotrope", true);
    let path = store::record(&f.root(), id(&saved)).unwrap();
    let directory = path.parent().unwrap();
    let external_directory = f.base.join("outside-directory");
    fs::rename(directory, &external_directory).unwrap();
    let status = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(directory)
        .arg(&external_directory)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(store::record(&f.root(), id(&saved)).is_err());
    assert!(f.store.get(&f.binding, id(&saved)).is_err());
    assert!(f.store.forget(&f.binding, id(&saved)).is_err());
    assert!(store::tombstone_ids(&f.root()).unwrap().is_empty());
    assert!(external_directory.join(path.file_name().unwrap()).exists());
}

#[test]
fn existing_library_is_preserved() {
    let f = Fixture::new("auto", "pilot");
    let path = f.binding.root.join("Library.base");
    let custom = "views: []\n# Personal table settings\n";
    fs::write(&path, custom).unwrap();
    views::ensure_library(&f.binding.root).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), custom);
}

#[test]
fn obsidian_properties_survive_read_and_approval() {
    let f = Fixture::new("inbox", "pilot");
    for properties in [
        "tags: [manual, project/rules]\naliases: [\"Порог чтения\"]\ncssclasses: compact\n",
        "tags:\n  - manual\n  - project/rules\naliases:\n- \"Порог чтения\"\ncssclasses:\n  - compact\n",
    ] {
        let entry = f.save(properties.lines().next().unwrap(), false);
        let path = store::record(&f.root(), id(&entry)).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let decorated = raw.replacen("---\n", &format!("---\n{properties}"), 1);
        fs::write(&path, &decorated).unwrap();
        assert_eq!(f.store.inbox(&f.binding).unwrap().as_array().unwrap().len(), 1);
        let approved = f.store.approve(&f.binding, id(&entry)).unwrap();
        assert_eq!(approved["body"], entry["body"]);
        assert!(fs::read_to_string(&path).unwrap().contains(properties));
        assert_eq!(f.store.get(&f.binding, id(&entry)).unwrap(), approved);
        assert!(!f.store.search(&f.binding, "tags", 5).unwrap().as_array().unwrap().is_empty());
    }
}

#[test]
fn obsidian_properties_cannot_hide_reserved_metadata_or_duplicate_keys() {
    let fields = BTreeMap::from([
        ("id".into(), "a".repeat(64)),
        ("kind".into(), "decision".into()),
        ("title".into(), "Правило".into()),
    ]);
    let raw = format::markdown(format::MEMORY, &fields, "Проверенное правило.");
    for invalid in [
        "tags: [one]\ntags: [two]\n",
        "tags:\n  id: invalid\n",
        "aliases:\n  - fstack_status: approved\n",
        "cssclasses: {id: invalid}\n",
        "aliases: []\nid: invalid\n",
    ] {
        let value = raw.replacen("---\n", &format!("---\n{invalid}"), 1);
        assert!(format::parse(&value, &"a".repeat(64), format::MEMORY, true).is_err());
    }
}

#[test]
fn inbox_manual_edits_dedup_and_scope() {
    let f = Fixture::new("inbox", "pilot");
    let v = f.save("Keep heliotrope", true);
    assert_eq!(v["status"], "inbox");
    assert_eq!(
        f.store.search(&f.binding, "heliotrope", 5).unwrap(),
        json!([])
    );
    let p = store::record(&f.root(), id(&v)).unwrap();
    let raw = fs::read_to_string(&p)
        .unwrap()
        .replace("Keep heliotrope", "Edited heliotrope");
    fs::write(&p, &raw).unwrap();
    let a = f.store.approve(&f.binding, id(&v)).unwrap();
    assert_eq!(a["body"], "Edited heliotrope");
    assert_eq!(f.save("Keep heliotrope", true), a);
    let other = Store::new(f.base.join("second")).unwrap();
    let b = other
        .setup(&f.vault, &f.repo, "pilot", Some("auto"))
        .unwrap();
    assert_eq!(
        other
            .save(&b, &candidate("Keep heliotrope"), true, None, None)
            .unwrap(),
        a
    );
    let alternate = other
        .setup(&f.vault, &f.repo, "other", Some("auto"))
        .unwrap();
    assert_eq!(
        other.search(&alternate, "heliotrope", 5).unwrap(),
        json!([])
    );
    assert!(other.get(&alternate, id(&v)).is_err());
    assert_eq!(
        f.store
            .search(&f.binding, "why should we keep heliotrope?", 5)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn exact_terms_rank_above_word_forms_and_newer_records_break_equal_scores() {
    let f = Fixture::new("auto", "pilot");
    let save = |title: &str, body| {
        f.store
            .save(
                &f.binding,
                &json!({"kind":"decision","title":title,"body":body}),
                true,
                None,
                None,
            )
            .unwrap()
    };
    let mut exact = [
        save("Публикации", "Первый пункт"),
        save("Публикации", "Второй пункт"),
    ];
    let inflected = save("Публикация", "Третий пункт");
    exact.sort_by(|a, b| id(a).cmp(id(b)));
    for (entry, when) in [
        (&exact[0], "2024-01-01"),
        (&exact[1], "2025-01-01"),
        (&inflected, "2026-01-01"),
    ] {
        let path = store::record(&f.root(), id(entry)).unwrap();
        let text = fs::read_to_string(&path).unwrap().replace(
            entry["created"].as_str().unwrap(),
            &format!("{when}T00:00:00+00:00"),
        );
        fs::write(path, text).unwrap();
    }
    let rows = f.store.search(&f.binding, "публикации", 3).unwrap();
    assert_eq!(
        rows.as_array().unwrap().iter().map(id).collect::<Vec<_>>(),
        [id(&exact[1]), id(&exact[0]), id(&inflected)]
    );
}

#[test]
fn superseding_forgetting_sync_and_damaged_records() {
    let f = Fixture::new("auto", "pilot");
    let first = f.save("First heliotrope", true);
    let mut c = candidate("Second heliotrope");
    c["supersedes"] = first["id"].clone();
    let second = f.store.save(&f.binding, &c, true, None, None).unwrap();
    assert!(f.store.get(&f.binding, id(&first)).is_err());
    let p = store::record(&f.root(), id(&second)).unwrap();
    let raw = fs::read_to_string(&p).unwrap();
    f.store.forget(&f.binding, id(&second)).unwrap();
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, &raw).unwrap();
    assert!(f.store.get(&f.binding, id(&first)).is_err());
    assert!(f.store.get(&f.binding, id(&second)).is_err());
    assert_eq!(
        f.store.search(&f.binding, "heliotrope", 5).unwrap(),
        json!([])
    );
    f.store.forget(&f.binding, id(&second)).unwrap();
    assert!(!p.exists());
    assert!(f.store.save(&f.binding, &c, true, None, None).is_err());
    let damaged = f.save("Damaged heliotrope", true);
    let p = store::record(&f.root(), id(&damaged)).unwrap();
    fs::write(&p, "not a memory note").unwrap();
    f.store.forget(&f.binding, id(&damaged)).unwrap();
    assert!(!p.exists());
}

#[test]
fn tombstone_recovery_keeps_approved_predecessor_hidden() {
    for captured in [false, true] {
        let f = Fixture::new("auto", "pilot");
        let first = f.save("First heliotrope", true);
        let mut c = candidate("Second heliotrope");
        c["supersedes"] = first["id"].clone();
        let second = f.store.save(&f.binding, &c, true, None, None).unwrap();
        let p = store::record(&f.root(), id(&second)).unwrap();
        let records = f.root().join("Records");
        let tomb = records.join(format!("{}-forgotten.json", id(&second)));
        fs::write(&tomb, json!({"id":id(&second)}).to_string()).unwrap();
        if captured {
            fs::rename(&p, records.join(format!(".fstack-forget-{}", id(&second)))).unwrap();
        }
        assert!(f.store.get(&f.binding, id(&first)).is_err());
        f.store.forget(&f.binding, id(&second)).unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(tomb).unwrap()).unwrap();
        assert_eq!(value["supersedes"], first["id"]);
        assert!(!p.exists());
        assert!(!p.parent().unwrap().exists());
        assert!(f.store.get(&f.binding, id(&first)).is_err());
    }
}

#[test]
fn draft_noise_and_large_views_do_not_block_approved_status() {
    let f = Fixture::new("auto", "token-authority");
    let first = f.save("First heliotrope", true);
    let mut c = candidate("Second heliotrope");
    c["supersedes"] = first["id"].clone();
    let second = f.store.save(&f.binding, &c, true, None, None).unwrap();
    let p = store::record(&f.root(), id(&second)).unwrap();
    let raw = fs::read_to_string(&p)
        .unwrap()
        .replacen("---\n", "---\ntags: [manual]\n", 1);
    fs::write(&p, raw).unwrap();
    assert!(f.store.get(&f.binding, id(&first)).is_err());
    let draft = f.save("Draft heliotrope", false);
    fs::write(
        store::record(&f.root(), id(&draft)).unwrap(),
        "arbitrary note",
    )
    .unwrap();
    assert!(f.store.get(&f.binding, id(&first)).is_err());
    fs::write(f.binding.root.join("Library.base"), "x".repeat(40000)).unwrap();
    let page = views::project_page(&f.root());
    let mut text = fs::read_to_string(&page).unwrap();
    text.push_str(&"Edited note ".repeat(10000));
    fs::write(&page, &text).unwrap();
    f.save("Third heliotrope", true);
    assert_eq!(fs::read_to_string(&page).unwrap(), text);
}

#[test]
fn search_and_recall_skip_invalid_record_bodies_without_reviving_predecessors() {
    let f = Fixture::new("auto", "pilot");
    let first = f.save("Original convention for heliotrope", true);
    let mut correction = candidate("Correction for heliotrope");
    correction["supersedes"] = first["id"].clone();
    let second = f
        .store
        .save(&f.binding, &correction, true, None, None)
        .unwrap();
    let other = f.save("Independent convention for heliotrope", true);
    let broken_draft = f.save("Draft for the first decision", false);
    let good_draft = f.save("Draft for the second decision", false);
    let path = store::record(&f.root(), id(&second)).unwrap();
    let raw = fs::read(&path).unwrap();
    let draft_path = store::record(&f.root(), id(&broken_draft)).unwrap();
    let draft_raw = fs::read(&draft_path).unwrap();
    for invalid_body in [vec![b'x'; 4001], vec![b'x'; 32768], vec![0xff]] {
        let mut edited = raw.clone();
        edited.extend(&invalid_body);
        fs::write(&path, edited).unwrap();
        let mut edited_draft = draft_raw.clone();
        edited_draft.extend(invalid_body);
        fs::write(&draft_path, edited_draft).unwrap();
        assert_eq!(
            f.store.search(&f.binding, "heliotrope", 5).unwrap(),
            json!([other])
        );
        assert_eq!(
            f.store.recall(&f.binding, "heliotrope", 5).unwrap(),
            json!([other])
        );
        assert_eq!(f.store.get(&f.binding, id(&other)).unwrap(), other);
        assert!(f.store.get(&f.binding, id(&first)).is_err());
        assert!(f.store.get(&f.binding, id(&second)).is_err());
        assert_eq!(f.store.inbox(&f.binding).unwrap(), json!([good_draft]));
    }
}

#[test]
fn project_page_selection_preserves_user_notes() {
    let f = Fixture::new("auto", "pilot");
    let named = views::project_page(&f.root());
    let old = f.root().join("Project.md");
    let raw = fs::read_to_string(&named).unwrap().replace(
        "[[Codex Memory/Projects|Все проекты]]",
        "[[Codex Memory/Home|Все проекты]]",
    ) + "\nMy own notes\n";
    fs::remove_file(&named).unwrap();
    fs::write(&old, &raw).unwrap();
    let v = f.save("Legacy-linked heliotrope", true);
    let record = store::record(&f.root(), id(&v)).unwrap();
    assert!(fs::read_to_string(&record)
        .unwrap()
        .contains("/Project|pilot]]"));
    assert_eq!(fs::read_to_string(&old).unwrap(), raw);
    assert!(!named.exists());
    fs::write(
        &named,
        "---\ntitle: mine\n---\n\nfstack_memory_project: pilot\n",
    )
    .unwrap();
    assert_eq!(views::current_page(&f.root()).unwrap(), Some(old));
}

#[test]
fn missing_pages_and_unmanaged_hub_fail_without_losing_records() {
    let f = Fixture::new("auto", "pilot");
    let v = f.save("Existing heliotrope", true);
    let page = views::project_page(&f.root());
    fs::remove_file(page).unwrap();
    assert!(f.store.get(&f.binding, id(&v)).is_ok());
    assert!(f
        .store
        .save(&f.binding, &candidate("New heliotrope"), true, None, None)
        .is_err());
    assert!(f.store.approve(&f.binding, id(&v)).is_err());
    fs::write(f.binding.root.join("Projects.md"), "my hub").unwrap();
    assert!(f.store.setup(&f.vault, &f.repo, "other", None).is_err());
    assert!(f.store.get(&f.binding, id(&v)).is_ok());
}

#[test]
fn concurrent_mutations_and_lock_failure_preserve_records() {
    let f = Fixture::new("inbox", "pilot");
    let v = f.save("Concurrent heliotrope", false);
    let start = std::sync::Barrier::new(8);
    let attempts = std::thread::scope(|scope| {
        let mut threads = Vec::new();
        for _ in 0..8 {
            threads.push(scope.spawn(|| {
                start.wait();
                f.store.approve(&f.binding, id(&v))
            }));
        }
        threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(attempts.iter().any(Result::is_ok));
    for attempt in attempts {
        let entry = match attempt {
            Ok(entry) => entry,
            Err(error) => {
                assert_eq!(error, "Timed out waiting for the local memory lock");
                f.store.approve(&f.binding, id(&v)).unwrap()
            }
        };
        assert_eq!(entry["status"], "memory");
        assert_eq!(entry["id"], v["id"]);
        assert_eq!(entry["body"], v["body"]);
    }
    assert_eq!(store::records(&f.root()).unwrap().len(), 1);
    let held = io::lock(&f.root(), &f.vault).unwrap();
    assert_eq!(
        f.store.approve(&f.binding, id(&v)).unwrap_err(),
        "Timed out waiting for the local memory lock"
    );
    drop(held);
    assert!(f.store.get(&f.binding, id(&v)).is_ok());
    let path = store::record(&f.root(), id(&v)).unwrap();
    let raw = fs::read_to_string(&path).unwrap();
    assert!(store::update(
        &path,
        "stale",
        &BTreeMap::new(),
        &BTreeMap::from([("fstack_status".into(), Some("approved".into()))])
    )
    .is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), raw);
}

#[test]
fn filesystem_rejects_links_and_atomic_replace_preserves_expected_content() {
    let f = Fixture::new("auto", "pilot");
    let p = f.root().join("safe.md");
    assert!(io::write(&p, "original", false, None).unwrap());
    assert!(!io::write(&p, "replacement", false, None).unwrap());
    assert!(io::write(&p, "replacement", true, Some("wrong")).is_err());
    assert_eq!(io::read(&p).unwrap().unwrap(), "original");
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let outside = f.base.join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, f.root().join("linked")).unwrap();
        assert!(io::directory(&f.root().join("linked"), true, false).is_err());
        symlink(&p, f.root().join("link.md")).unwrap();
        assert!(io::read(&f.root().join("link.md")).is_err());
        assert!(io::write(&f.root().join("link.md"), "bad", true, None).is_err());
    }
}

#[test]
fn git_registered_worktrees_share_binding_and_forged_pointers_fail() {
    let f = Fixture::new("auto", "pilot");
    let prior_path = scope::binding_path(&f.store.state, &f.repo).unwrap();
    let prior = f
        .store
        .setup(&f.vault, &f.repo, "pilot", Some("inbox"))
        .unwrap();
    let saved = f
        .store
        .save(
            &prior,
            &candidate("A remembered convention"),
            true,
            None,
            None,
        )
        .unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&f.repo)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet"]);
    git(&[
        "-c",
        "user.name=Memory Test",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "--allow-empty",
        "-m",
        "init",
    ]);
    let linked = f.base.join("linked worktree");
    let git_linked = io::normalized(&linked);
    git(&["worktree", "add", "--detach", &git_linked]);
    let nested = linked.join("nested");
    fs::create_dir(&nested).unwrap();
    fs::create_dir(nested.join(".git")).unwrap();
    for cwd in [&nested, &f.repo] {
        let restored = f.store.auto_resolve(cwd).unwrap().unwrap();
        assert_eq!(restored.project, "pilot");
        assert_eq!(restored.mode, "inbox");
        assert_eq!(restored.generation, prior.generation);
        assert_eq!(f.store.inbox(&restored).unwrap(), json!([saved]));
        assert_eq!(
            scope::binding_path(&f.store.state, cwd).unwrap(),
            prior_path
        );
    }
    let fresh_state = f.base.join("fresh-state");
    assert_eq!(
        scope::binding_path(&fresh_state, &f.repo).unwrap(),
        scope::binding_path(&fresh_state, &nested).unwrap()
    );
    for worktree in [&f.repo, &linked] {
        let state_in_worktree = worktree.join("profile/fstack/memory");
        assert!(io::profile_state(&state_in_worktree, true).is_err());
        assert!(!state_in_worktree.exists());
    }
    let forged = f.base.join("forged");
    fs::create_dir(&forged).unwrap();
    fs::copy(linked.join(".git"), forged.join(".git")).unwrap();
    assert!(f.store.resolve(&forged).is_err());
    assert!(f.store.disable(&linked).unwrap());
    assert!(f.store.auto_resolve(&f.repo).unwrap().is_none());
    assert!(f.store.auto_resolve(&linked).unwrap().is_none());
}

#[test]
fn large_tombstone_history_deduplicates_markdown_and_json_entries() {
    let f = Fixture::new("auto", "pilot");
    let records = f.root().join("Records");
    let forgotten = f.root().join("Forgotten");
    fs::create_dir(&forgotten).unwrap();
    for n in 0..4097 {
        let id = format!("{n:064x}");
        fs::write(
            records.join(format!("{id}-forgotten.json")),
            json!({"id":id}).to_string(),
        )
        .unwrap();
    }
    assert_eq!(store::tombstone_ids(&f.root()).unwrap().len(), 4097);
    let first = format!("{:064x}", 0);
    f.marker("Forgotten", &first, None);
    assert_eq!(store::tombstone_ids(&f.root()).unwrap().len(), 4097);
    let next = format!("{:064x}", 4097);
    f.marker("Forgotten", &next, None);
    fs::write(
        records.join(format!("{next}-forgotten.json")),
        json!({"id":next}).to_string(),
    )
    .unwrap();
    assert_eq!(store::tombstone_ids(&f.root()).unwrap().len(), 4098);
    let latest = f.save("Can still forget a new decision", true);
    f.store.forget(&f.binding, id(&latest)).unwrap();
    assert_eq!(store::tombstone_ids(&f.root()).unwrap().len(), 4099);
    assert!(f.store.get(&f.binding, id(&latest)).is_err());
}

#[test]
fn large_record_history_stays_readable_and_writable() {
    let f = Fixture::new("auto", "pilot");
    let root = f.root();
    let records = root.join("Records");
    let saved = f.save("Keep readable heliotrope", true);
    let draft = f.save("Draft from another device", false);
    let contents = fs::read_to_string(store::record(&root, id(&draft)).unwrap()).unwrap();
    for n in 0..4094 {
        let synced_id = format!("{n:064x}");
        let directory = records.join(&synced_id);
        fs::create_dir(&directory).unwrap();
        fs::write(
            directory.join("Heliotrope convention.md"),
            contents.replace(id(&draft), &synced_id),
        )
        .unwrap();
    }
    fs::write(records.join("Personal note.md"), "Other vault data").unwrap();
    assert_eq!(store::records(&root).unwrap().len(), 4096);
    let next = f.save("First writable record past the previous limit", true);
    let last = f.save("Next writable record", true);
    assert_eq!(f.save("Next writable record", true)["id"], last["id"]);
    assert_eq!(store::records(&root).unwrap().len(), 4098);
    assert_eq!(f.store.get(&f.binding, id(&saved)).unwrap(), saved);
    assert_eq!(f.store.search(&f.binding, "readable", 1).unwrap()[0], saved);
    f.store.forget(&f.binding, id(&draft)).unwrap();
    f.store.forget(&f.binding, id(&last)).unwrap();
    assert_eq!(f.store.get(&f.binding, id(&next)).unwrap(), next);
    assert!(f.store.get(&f.binding, id(&last)).is_err());
}

#[test]
fn promotion_rejects_oversize_without_losing_editable_inbox() {
    let f = Fixture::new("inbox", "pilot");
    let v = f.save("Keep editable heliotrope", false);
    let p = store::record(&f.root(), id(&v)).unwrap();
    let mut raw = fs::read_to_string(&p).unwrap();
    raw.push_str(&"\n".repeat(32760 - raw.len()));
    fs::write(&p, &raw).unwrap();
    assert!(f.store.approve(&f.binding, id(&v)).is_err());
    assert_eq!(fs::read_to_string(&p).unwrap(), raw);
    assert_eq!(f.store.inbox(&f.binding).unwrap()[0]["status"], "inbox");
    fs::write(&p, raw.trim_end().to_owned() + "\n").unwrap();
    assert!(f.store.approve(&f.binding, id(&v)).is_ok());
    fs::write(&p, [0xff, 0xfe, 0x00]).unwrap();
    f.store.forget(&f.binding, id(&v)).unwrap();
    assert!(!p.exists());
}

#[test]
fn markdown_tombstones_respect_json_confirmation_without_rewriting() {
    let f = Fixture::new("auto", "pilot");
    let first = f.save("First heliotrope", true);
    let second = f.save("Second heliotrope", true);
    let tomb = f
        .root()
        .join("Records")
        .join(format!("{}-forgotten.json", id(&second)));
    fs::write(&tomb, json!({"id":id(&second)}).to_string()).unwrap();
    f.marker("Forgotten", id(&second), Some(id(&first)));
    assert!(f.store.get(&f.binding, id(&first)).is_err());
    let v: Value = serde_json::from_str(&fs::read_to_string(&tomb).unwrap()).unwrap();
    assert!(v.get("supersedes").is_none());
    fs::write(
        &tomb,
        json!({"id":id(&second),"supersedes":null}).to_string(),
    )
    .unwrap();
    let v: Value = serde_json::from_str(&fs::read_to_string(&tomb).unwrap()).unwrap();
    assert!(v["supersedes"].is_null());
    assert!(f.store.get(&f.binding, id(&first)).is_ok());
}

#[cfg(unix)]
#[test]
fn state_symlinks_non_utf8_paths_and_unsafe_git_path_fail_closed() {
    use std::{
        ffi::OsString,
        os::unix::{ffi::OsStringExt, fs::symlink},
    };
    let f = Fixture::new("auto", "pilot");
    let outside = f.base.join("outside");
    fs::create_dir(&outside).unwrap();
    let state = f.base.join("linked-state");
    symlink(&outside, &state).unwrap();
    assert!(io::state(&state, &f.repo, true).is_err());
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    let bad = f.base.join(OsString::from_vec(vec![0xff]));
    assert!(io::absolute(&bad).is_err());
}

#[test]
fn git_executable_ignores_workspace_and_relative_search_paths() {
    let f = Fixture::new("auto", "pilot");
    let name = if cfg!(windows) { "git.exe" } else { "git" };
    fs::write(f.repo.join(name), "not git").unwrap();
    let mut paths = vec![PathBuf::new(), PathBuf::from("."), f.repo.clone()];
    let untrusted = std::env::join_paths(&paths).unwrap();
    assert!(scope::executable(&f.repo, &untrusted).is_err());
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let exe = scope::executable(&f.repo, &std::env::join_paths(paths).unwrap()).unwrap();
    assert!(exe.is_absolute() && !exe.starts_with(&f.repo));
}

#[cfg(windows)]
#[test]
fn windows_path_prefix_case_and_separators_preserve_scope() {
    let ordinary = Path::new(r"C:\Users\Example\Repo");
    let verbatim = Path::new(r"\\?\c:\users\example\repo");
    assert!(io::equal(ordinary, verbatim));
    assert!(io::within(
        Path::new(r"\\?\C:\USERS\EXAMPLE\REPO\git.exe"),
        ordinary
    ));
    assert!(!io::within(
        Path::new(r"C:\Users\Example\Repo-other\git.exe"),
        ordinary
    ));
    assert_eq!(io::normalized(ordinary), io::normalized(verbatim));
    assert!(io::equal(
        Path::new(r"\\?\UNC\server\share\repo"),
        Path::new(r"\\SERVER\SHARE\REPO")
    ));
    let f = Fixture::new("auto", "pilot");
    let fake = f.repo.join("git.exe");
    fs::write(&fake, "not Git").unwrap();
    let uppercase = PathBuf::from(f.repo.to_string_lossy().to_uppercase());
    assert!(scope::executable(&f.repo, &std::env::join_paths([uppercase]).unwrap()).is_err());
    let junction = f.base.join("outside-junction");
    let result = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&junction)
        .arg(&f.repo)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(scope::executable(&f.repo, &std::env::join_paths([junction]).unwrap()).is_err());
}

#[test]
fn cross_profile_locking_hides_forgotten_records() {
    let f = Fixture::new("inbox", "pilot");
    let v = f.save("Current heliotrope", false);
    let other_store = Store::new(f.base.join("second-profile")).unwrap();
    let b = other_store
        .setup(&f.vault, &f.repo, "pilot", Some("auto"))
        .unwrap();
    std::thread::scope(|scope| {
        let save = scope
            .spawn(|| other_store.save(&b, &candidate("Current heliotrope"), true, None, None));
        let remove = scope.spawn(|| f.store.forget(&f.binding, id(&v)));
        let _ = save.join().unwrap();
        remove.join().unwrap().unwrap();
    });
    assert!(other_store.get(&b, id(&v)).is_err());
    assert!(f.store.get(&f.binding, id(&v)).is_err());
    assert!(store::record(&f.root(), id(&v)).is_err());
}

#[test]
fn history_links_scoped_approved_versions_and_diffs_without_forgotten_content() {
    let f = Fixture::new("auto", "pilot");
    let a = f
        .store
        .save(
            &f.binding,
            &json!({
                "kind":"decision","title":"Heliotrope convention","body":"Default\nUbuntu\nRelease",
                "source":"workflow.yml @ v1",
            }),
            true,
            None,
            None,
        )
        .unwrap();
    let b = f.store.save(&f.binding, &json!({
        "kind":"correction","title":"Heliotrope convention","body":"Default\nWindows\nRelease",
        "source":"workflow.yml @ v2","supersedes":id(&a),
    }), true, Some("session"), Some("update")).unwrap();
    let draft = f.store.save(&f.binding, &json!({
        "kind":"correction","title":"Heliotrope convention","body":"A proposal","supersedes":id(&b),
    }), false, None, None).unwrap();
    assert_eq!(
        f.store.search(&f.binding, "Heliotrope", 5).unwrap(),
        json!([b])
    );
    assert!(f.store.get(&f.binding, id(&a)).is_err());
    assert!(f.store.history(&f.binding, id(&draft), 5).is_err());
    assert!(f.store.history(&f.binding, id(&b), 11).is_err());
    let from_new = f.store.history(&f.binding, id(&b), 5).unwrap();
    let from_old = f.store.history(&f.binding, id(&a), 5).unwrap();
    assert_eq!(from_new["entries"], json!([b, a]));
    assert_eq!(from_old["entries"], json!([a, b]));
    assert_eq!(from_new["changes"], from_old["changes"]);
    assert_eq!(
        from_new["changes"][0],
        json!({
            "from":id(&a),"to":id(&b),"fields":{
                "kind":{"from":"decision","to":"correction"},
                "source":{"from":"workflow.yml @ v1","to":"workflow.yml @ v2"},
                "body":{"start_line":2,"removed":["Ubuntu"],"added":["Windows"]},
            }
        })
    );
    assert_eq!(
        f.store.history(&f.binding, id(&b), 1).unwrap(),
        json!({"entries":[b],"changes":[],"truncated":true})
    );
    let foreign = f
        .store
        .setup(&f.vault, &f.repo, "other", Some("auto"))
        .unwrap();
    assert!(f.store.history(&foreign, id(&a), 5).is_err());

    let old_path = store::record(&f.root(), id(&a)).unwrap();
    let stale = fs::read(&old_path).unwrap();
    f.store.forget(&f.binding, id(&a)).unwrap();
    fs::create_dir_all(old_path.parent().unwrap()).unwrap();
    fs::write(&old_path, stale).unwrap();
    assert!(f.store.history(&f.binding, id(&a), 5).is_err());
    let after_forget = f.store.history(&f.binding, id(&b), 5).unwrap();
    assert_eq!(after_forget["entries"].as_array().unwrap().len(), 1);
    assert_eq!(after_forget["changes"], json!([]));
    assert!(!after_forget.to_string().contains("Ubuntu"));
    assert!(!after_forget.to_string().contains(id(&a)));
}

#[cfg(windows)]
#[test]
fn windows_publish_preserves_atomic_new_and_replace_for_long_unicode_paths() {
    use std::os::windows::ffi::OsStrExt;
    let f = Fixture::new("auto", "pilot");
    let component = "проверка".repeat(8);
    let mut parent = f.vault.join(&component);
    let filename = "заметка.md";
    while parent.join(filename).as_os_str().encode_wide().count() < 260 {
        parent.push(&component);
    }
    fs::create_dir_all(&parent).unwrap();
    let target = parent.join(filename);
    assert!(target.as_os_str().encode_wide().count() >= 260);
    assert!(io::write(&target, "one", false, None).unwrap());
    assert!(!io::write(&target, "lost", false, None).unwrap());
    assert_eq!(io::read(&target).unwrap().as_deref(), Some("one"));
    assert!(io::write(&target, "two", true, Some("one")).unwrap());
    assert_eq!(io::read(&target).unwrap().as_deref(), Some("two"));

    for (path, expected) in [
        (
            r"\\server\share\каталог\new.md",
            r"\\?\UNC\server\share\каталог\new.md",
        ),
        (
            r"//server/share/каталог/new.md",
            r"\\?\UNC\server\share\каталог\new.md",
        ),
        (
            r"\\server/share/каталог/new.md",
            r"\\?\UNC\server\share\каталог\new.md",
        ),
        (r"каталог/new.md", r"каталог/new.md"),
        (r"\\?\C:\каталог\new.md", r"\\?\C:\каталог\new.md"),
        (r"C:/каталог/new.md", r"\\?\C:\каталог\new.md"),
    ] {
        let units = io::windows_publish_path(Path::new(path));
        assert_eq!(
            units,
            expected.encode_utf16().chain(Some(0)).collect::<Vec<_>>()
        );
    }
}
