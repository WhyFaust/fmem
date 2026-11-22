use super::*;

#[test]
fn worker_checks_current_binding_before_accepting_a_scope() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let vault = root.join("vault");
    let cwd = root.join("workspace");
    std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
    std::fs::create_dir(&cwd).unwrap();
    let store = Store::new(root.join("state")).unwrap();
    let original = store.setup(&vault, &cwd, "first", Some("auto")).unwrap();
    let original_scope = Scope::new(&cwd, &original);
    assert!(original_scope.resolve(&store).unwrap().is_some());
    run(&store, &cwd, &original).unwrap();
    assert!(!store.state.join("semantic-worker").exists());

    let changed = store.setup(&vault, &cwd, "second", Some("auto")).unwrap();
    assert!(original_scope.resolve(&store).unwrap().is_none());
    let changed_scope = Scope::new(&cwd, &changed);
    assert!(changed_scope.resolve(&store).unwrap().is_some());

    let another = root.join("another-workspace");
    std::fs::create_dir(&another).unwrap();
    assert!(Scope::new(&another, &changed)
        .resolve(&store)
        .unwrap()
        .is_none());
    store.disable(&cwd).unwrap();
    assert!(changed_scope.resolve(&store).unwrap().is_none());
}
