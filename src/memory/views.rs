use super::{format, io, Result};
use std::path::{Path, PathBuf};
pub const VAULT_FILES: &[&str] = &["Home.md", "Library.base", "Projects.base", "Общие.md"];
pub fn asset(name: &str) -> Result<&'static str> {
    match name {
        "Home.md" => Ok(include_str!("../../assets/vault/Home.md")),
        "Library.base" => Ok(include_str!("../../assets/vault/Library.base")),
        "Projects.base" => Ok(include_str!("../../assets/vault/Projects.base")),
        "Общие.md" => Ok(include_str!("../../assets/vault/Preferences.md")),
        "graph.json" => Ok(include_str!("../../assets/vault/graph.json")),
        "fmem.css" => Ok(include_str!("../../assets/vault/fmem.css")),
        "Projects.md" => Ok(include_str!("../../assets/vault/Projects.md")),
        "Project.md" => Ok(include_str!("../../assets/vault/Project.md")),
        _ => Err("Unknown memory vault asset".into()),
    }
}
pub fn marker(text: Option<&str>, marker: &str) -> bool {
    let Some(text) = text else { return false };
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        return false;
    }
    let mut found = false;
    for line in lines {
        if line == "---" {
            return found;
        }
        found |= line == marker;
    }
    false
}

pub fn project_page(root: &Path) -> PathBuf {
    let name = root.file_name().unwrap().to_string_lossy();
    root.join(format!(
        "{}.md",
        if name == "project" { "Project" } else { &name }
    ))
}
pub fn project_marker(raw: &str, project: &str) -> bool {
    let Some((meta, _)) = raw
        .strip_prefix("---\n")
        .and_then(|s| s.split_once("\n---\n"))
    else {
        return false;
    };
    let values = meta
        .lines()
        .filter_map(|l| l.split_once(':'))
        .filter(|(k, _)| k.trim_end() == "fstack_memory_project")
        .map(|(_, v)| v.trim())
        .collect::<Vec<_>>();
    values.len() == 1
        && format::matches(
            &format!(r#"\A(?:{project}|"{project}"|'{project}')(?:[ \t]+#.*)?\z"#),
            values[0],
        )
}
fn has_marker(path: &Path, project: &str) -> Result<bool> {
    let Some(data) = io::prefix(path, 32768)? else {
        return Ok(false);
    };
    let re = regex::bytes::Regex::new(r"(?-u)\A(?:\xef\xbb\xbf)?---\r?\n(?:[^\n]*\n)*?---\r?\n")
        .unwrap();
    let Some(m) = re.find(&data) else {
        return Ok(false);
    };
    let text = std::str::from_utf8(m.as_bytes())
        .map_err(io::err)?
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n");
    Ok(project_marker(&text, project))
}
pub fn current_page(root: &Path) -> Result<Option<PathBuf>> {
    let project = root.file_name().unwrap().to_string_lossy();
    let p = project_page(root);
    if has_marker(&p, &project)? {
        return Ok(Some(p));
    }
    let old = root.join("Project.md");
    if old != p && has_marker(&old, &project)? {
        return Ok(Some(old));
    }
    Ok(None)
}
pub fn project_link(page: &Path) -> String {
    let project = page
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy();
    format!(
        "[[Codex Memory/Projects/{project}/{}|{project}]]",
        page.file_stem().unwrap().to_string_lossy()
    )
}
pub fn current_link(root: &Path) -> Result<String> {
    if super::shared::is_root(root) {
        return Ok(super::shared::LINK.into());
    }
    current_page(root)?
        .map(|p| project_link(&p))
        .ok_or("Memory project page is unavailable".into())
}
pub fn ensure_hub(root: &Path) -> Result<()> {
    let p = root.join("Projects.md");
    if io::read(&p)?.is_none() {
        io::write(&p, asset("Projects.md")?, false, None)?;
    }
    if !marker(io::read(&p)?.as_deref(), "fstack_memory_projects: 1") {
        return Err("Memory projects hub is occupied".into());
    }
    Ok(())
}
pub fn ensure_library(root: &Path) -> Result<()> {
    let destination = root.join("Library.base");
    if io::read(&destination)?.is_none() {
        io::write(&destination, asset("Library.base")?, false, None)?;
    }
    Ok(())
}

pub fn ensure_style(vault: &Path) -> Result<()> {
    let config = vault.join(".obsidian");
    if !io::directory(&config, false, false)? {
        return Err("Obsidian configuration directory is unavailable".into());
    }
    let snippets = config.join("snippets");
    io::directory(&snippets, true, false)?;
    let stylesheet = snippets.join("fmem.css");
    let style = asset("fmem.css")?;
    io::write(&stylesheet, style, false, None)?;
    if io::read(&stylesheet)?.as_deref() != Some(style) {
        return Ok(());
    }
    let appearance = config.join("appearance.json");
    let original = io::read(&appearance)?;
    let mut settings = match &original {
        Some(text) => super::read_json(text.as_bytes(), 32768)?,
        None => serde_json::json!({}),
    };
    let snippets = settings
        .as_object_mut()
        .ok_or("Invalid Obsidian appearance settings")?
        .entry("enabledCssSnippets")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .ok_or("Invalid Obsidian snippet settings")?;
    if snippets.iter().any(|value| !value.is_string()) {
        return Err("Invalid Obsidian snippet name".into());
    }
    if snippets.iter().any(|value| value == "fmem") {
        return Ok(());
    }
    snippets.push(serde_json::json!("fmem"));
    let text = serde_json::to_string_pretty(&settings).map_err(io::err)? + "\n";
    io::write(&appearance, &text, original.is_some(), original.as_deref())?;
    Ok(())
}

pub fn ensure_interface(vault: &Path) {
    let _ = ensure_style(vault);
    let _ = ensure_graph(vault);
}

fn ensure_graph(vault: &Path) -> Result<()> {
    let config = vault.join(".obsidian");
    if !io::directory(&config, false, false)? {
        return Err("Obsidian configuration directory is unavailable".into());
    }
    let path = config.join("bookmarks.json");
    let original = io::read(&path)?;
    let mut settings = match &original {
        Some(text) => super::read_json(text.as_bytes(), 32768)?,
        None => serde_json::json!({}),
    };
    let items = settings
        .as_object_mut()
        .ok_or("Invalid Obsidian bookmarks settings")?
        .entry("items")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .ok_or("Invalid Obsidian bookmarks list")?;
    if has_memory_graph(items) {
        return Ok(());
    }
    items.push(serde_json::json!({
        "type": "graph",
        "title": "Память",
        "ctime": chrono::Utc::now().timestamp_millis(),
        "options": super::read_json(asset("graph.json")?.as_bytes(), 32768)?
    }));
    let text = serde_json::to_string_pretty(&settings).map_err(io::err)? + "\n";
    io::write(&path, &text, original.is_some(), original.as_deref())?;
    Ok(())
}

fn has_memory_graph(items: &[serde_json::Value]) -> bool {
    items.iter().any(|item| {
        item["type"] == "graph" && item["title"] == "Память"
            || item["type"] == "group"
                && item["items"]
                    .as_array()
                    .is_some_and(|nested| has_memory_graph(nested))
    })
}

#[cfg(test)]
mod tests {
    use super::super::Store;
    use super::*;
    use serde_json::json;
    use std::fs;

    fn style_fixture() -> (tempfile::TempDir, Store, PathBuf, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let vault = root.join("vault");
        let repo = root.join("repo");
        fs::create_dir_all(vault.join(".obsidian")).unwrap();
        fs::create_dir(&repo).unwrap();
        let store = Store::new(root.join("state")).unwrap();
        (temporary, store, vault, repo)
    }

    #[test]
    fn setup_installs_dashboard_style_and_preserves_appearance_options() {
        let (_temporary, store, vault, repo) = style_fixture();
        let appearance = vault.join(".obsidian/appearance.json");
        let mut expected = json!({
            "theme":"moonstone", "cssTheme":"Personal theme", "baseFontSize":18,
            "accentColor":"#123456", "enabledCssSnippets":["tables", "tasks"],
            "custom":{"nested":[1,2]}
        });
        fs::write(&appearance, expected.to_string()).unwrap();

        let binding = store.setup(&vault, &repo, "sample", None).unwrap();

        assert_eq!(
            fs::read_to_string(vault.join(".obsidian/snippets/fmem.css")).unwrap(),
            asset("fmem.css").unwrap()
        );
        expected["enabledCssSnippets"] = json!(["tables", "tasks", "fmem"]);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(appearance).unwrap()).unwrap(),
            expected
        );
        assert_eq!(
            store.bound(&repo).unwrap().unwrap().generation,
            binding.generation
        );
    }

    #[test]
    fn setup_preserves_existing_dashboard_style_and_is_idempotent() {
        let (_temporary, store, vault, repo) = style_fixture();
        let snippets = vault.join(".obsidian/snippets");
        fs::create_dir(&snippets).unwrap();
        let stylesheet = snippets.join("fmem.css");
        let custom = b".fmem-dashboard { color: inherit; }\r\n";
        fs::write(&stylesheet, custom).unwrap();
        let appearance = vault.join(".obsidian/appearance.json");
        let original = "{\"enabledCssSnippets\":[\"tables\"],\"theme\":\"moonstone\"}";
        fs::write(&appearance, original).unwrap();

        store.setup(&vault, &repo, "sample", None).unwrap();
        let first = fs::read(&appearance).unwrap();
        assert_eq!(first, original.as_bytes());
        store.setup(&vault, &repo, "sample", None).unwrap();

        assert_eq!(fs::read(stylesheet).unwrap(), custom);
        assert_eq!(fs::read(appearance).unwrap(), first);
    }

    #[test]
    fn graph_bookmark_preserves_existing_bookmarks_and_global_graph() {
        let (_temporary, store, vault, repo) = style_fixture();
        let path = vault.join(".obsidian/bookmarks.json");
        let personal = serde_json::json!({
            "type":"group", "title":"Работа", "items":[{
                "type":"file", "path":"My notes.md", "ctime":1234
            }]
        });
        fs::write(
            &path,
            serde_json::json!({"items":[personal],"custom":true}).to_string(),
        )
        .unwrap();
        let graph = vault.join(".obsidian/graph.json");
        let original = "{\"search\":\"tag:personal\",\"scale\":0.75}";
        fs::write(&graph, original).unwrap();

        store.setup(&vault, &repo, "sample", None).unwrap();

        let first = fs::read(&path).unwrap();
        let settings: serde_json::Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(settings["items"][0], personal);
        assert_eq!(settings["custom"], true);
        assert_eq!(settings["items"].as_array().unwrap().len(), 2);
        assert_eq!(settings["items"][1]["type"], "graph");
        assert_eq!(settings["items"][1]["title"], "Память");
        assert_eq!(settings["items"][1]["options"]["hideUnresolved"], true);
        assert_eq!(fs::read_to_string(&graph).unwrap(), original);

        store.setup(&vault, &repo, "sample", None).unwrap();
        assert_eq!(fs::read(path).unwrap(), first);
    }

    #[test]
    fn customized_graph_bookmark_is_preserved_in_nested_group() {
        let (_temporary, store, vault, repo) = style_fixture();
        let path = vault.join(".obsidian/bookmarks.json");
        let original = serde_json::json!({"items":[{
            "type":"group", "title":"Мои виды", "items":[{
                "type":"graph", "title":"Память", "options":{"search":"my query"}
            }]
        }]})
        .to_string();
        fs::write(&path, &original).unwrap();

        store.setup(&vault, &repo, "sample", None).unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn malformed_bookmarks_do_not_break_memory_setup() {
        for original in ["broken", "[]", "{\"items\":null}", "{\"items\":{}}"] {
            let (_temporary, store, vault, repo) = style_fixture();
            let path = vault.join(".obsidian/bookmarks.json");
            fs::write(&path, original).unwrap();

            store.setup(&vault, &repo, "sample", None).unwrap();

            assert_eq!(fs::read_to_string(path).unwrap(), original);
            assert!(store.bound(&repo).unwrap().is_some());
        }
    }

    #[cfg(unix)]
    #[test]
    fn linked_bookmarks_are_not_modified() {
        let (temporary, store, vault, repo) = style_fixture();
        let outside = temporary.path().join("personal-bookmarks.json");
        fs::write(&outside, "{\"items\":[]}").unwrap();
        std::os::unix::fs::symlink(&outside, vault.join(".obsidian/bookmarks.json")).unwrap();

        store.setup(&vault, &repo, "sample", None).unwrap();

        assert_eq!(fs::read_to_string(outside).unwrap(), "{\"items\":[]}");
        assert!(store.bound(&repo).unwrap().is_some());
    }

    #[test]
    fn invalid_appearance_is_preserved_without_breaking_binding() {
        for invalid in [
            "{broken",
            "[]",
            "null",
            "{\"enabledCssSnippets\":null}",
            "{\"enabledCssSnippets\":\"tables\"}",
            "{\"enabledCssSnippets\":[\"tables\",false]}",
            "{\"theme\":\"light\",\"theme\":\"dark\"}",
        ] {
            let (_temporary, store, vault, repo) = style_fixture();
            let appearance = vault.join(".obsidian/appearance.json");
            fs::write(&appearance, invalid).unwrap();

            let binding = store.setup(&vault, &repo, "sample", None).unwrap();

            assert_eq!(fs::read_to_string(appearance).unwrap(), invalid);
            assert!(vault.join(".obsidian/snippets/fmem.css").is_file());
            assert_eq!(
                store.bound(&repo).unwrap().unwrap().generation,
                binding.generation
            );
        }
    }

    #[test]
    fn unavailable_snippet_directory_does_not_break_binding_or_enable_missing_style() {
        let (_temporary, store, vault, repo) = style_fixture();
        fs::write(vault.join(".obsidian/snippets"), "occupied").unwrap();
        let appearance = vault.join(".obsidian/appearance.json");
        let original = "{\"enabledCssSnippets\":[\"tables\"],\"theme\":\"moonstone\"}";
        fs::write(&appearance, original).unwrap();

        let binding = store.setup(&vault, &repo, "sample", None).unwrap();

        assert_eq!(fs::read_to_string(appearance).unwrap(), original);
        assert_eq!(
            store.bound(&repo).unwrap().unwrap().generation,
            binding.generation
        );
    }

    #[cfg(unix)]
    #[test]
    fn linked_appearance_is_not_modified() {
        let (temporary, store, vault, repo) = style_fixture();
        let target = temporary.path().join("outside.json");
        let original = "{\"enabledCssSnippets\":[\"tables\"]}";
        fs::write(&target, original).unwrap();
        std::os::unix::fs::symlink(&target, vault.join(".obsidian/appearance.json")).unwrap();

        store.setup(&vault, &repo, "sample", None).unwrap();

        assert_eq!(fs::read_to_string(target).unwrap(), original);
        assert!(
            fs::symlink_metadata(vault.join(".obsidian/appearance.json"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(store.bound(&repo).unwrap().is_some());
    }

    #[test]
    fn vault_marker_accepts_windows_line_endings_and_requires_frontmatter() {
        let source = asset("Home.md").unwrap();
        assert!(marker(Some(source), "fstack_memory_home: 1"));
        assert!(marker(
            Some(&source.replace("\r\n", "\n").replace('\n', "\r\n")),
            "fstack_memory_home: 1"
        ));
        assert!(!marker(
            Some("---\r\nfstack_memory_home: 1\r\n"),
            "fstack_memory_home: 1"
        ));
        assert!(!marker(
            Some("text\n---\nfstack_memory_home: 1\n---\n"),
            "fstack_memory_home: 1"
        ));
    }

    #[test]
    fn setup_installs_current_views_and_preserves_existing_notes() {
        use super::super::Store;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let vault = root.join("vault");
        let repo = root.join("repo");
        fs::create_dir_all(vault.join(".obsidian")).unwrap();
        fs::create_dir(&repo).unwrap();
        let store = Store::new(root.join("state")).unwrap();
        let binding = store.setup(&vault, &repo, "sample", Some("inbox")).unwrap();
        for &name in VAULT_FILES {
            assert_eq!(
                fs::read_to_string(binding.root.join(name)).unwrap(),
                asset(name).unwrap()
            );
        }
        let home = binding.root.join("Home.md");
        let project = project_page(&store.root(&binding).unwrap());
        let original = fs::read_to_string(&project).unwrap();
        assert!(project_marker(&original, "sample"));
        let edited_home = format!("{}\nМои рабочие ссылки.\n", asset("Home.md").unwrap());
        let edited_project = format!("{original}\nПлан проверки команды.\n");
        fs::write(&home, &edited_home).unwrap();
        fs::write(&project, &edited_project).unwrap();

        store.setup(&vault, &repo, "sample", Some("inbox")).unwrap();
        assert_eq!(fs::read_to_string(home).unwrap(), edited_home);
        assert_eq!(fs::read_to_string(project).unwrap(), edited_project);
    }

    #[test]
    fn setup_does_not_claim_an_occupied_shared_tree() {
        let (_temporary, store, vault, repo) = style_fixture();
        let memory = vault.join("Codex Memory");
        fs::create_dir_all(memory.join("Shared/Records")).unwrap();
        fs::write(memory.join("Home.md"), asset("Home.md").unwrap()).unwrap();
        let personal = memory.join("Shared/Records/personal.md");
        fs::write(&personal, "An existing note").unwrap();

        assert_eq!(
            store.setup(&vault, &repo, "sample", None).unwrap_err(),
            "Shared preferences directory is occupied"
        );
        assert!(!memory.join("Общие.md").exists());
        assert_eq!(fs::read_to_string(personal).unwrap(), "An existing note");
        assert!(store.bound(&repo).unwrap().is_none());
    }
}
