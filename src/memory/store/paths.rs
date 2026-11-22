use super::{fmt, io, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub fn record_id(path: &Path) -> Result<&str> {
    let directory = path.parent().ok_or("Invalid memory record path")?;
    if directory.parent().and_then(Path::file_name) != Some("Records".as_ref()) {
        return Err("Invalid memory record path".into());
    }
    fmt::id(
        directory
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("Invalid memory record path")?,
    )
}

pub(crate) fn find_record(root: &Path, id: &str) -> Result<Option<PathBuf>> {
    let directory = root.join("Records").join(fmt::id(id)?);
    if !io::directory(&directory, false, false)? {
        return Ok(None);
    }
    let mut found = None;
    for entry in fs::read_dir(&directory).map_err(io::err)? {
        let path = entry.map_err(io::err)?.path();
        if !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        let basename = path
            .file_stem()
            .and_then(|name| name.to_str())
            .ok_or("Memory paths require UTF-8")?;
        fmt::validate_record_basename(basename)?;
        if !io::file(&path)? {
            continue;
        }
        if found.replace(path).is_some() {
            return Err("Memory record directory contains multiple Markdown files".into());
        }
    }
    Ok(found)
}

pub fn record(root: &Path, id: &str) -> Result<PathBuf> {
    find_record(root, id)?.ok_or("Memory entry is unavailable".into())
}

pub fn records(root: &Path) -> Result<Vec<PathBuf>> {
    let directory = root.join("Records");
    if !io::directory(&directory, false, false)? {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(&directory).map_err(io::err)? {
        let entry = entry.map_err(io::err)?;
        let name = entry.file_name();
        let Some(id) = name.to_str().filter(|name| fmt::id(name).is_ok()) else {
            continue;
        };
        if let Some(path) = find_record(root, id)? {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

pub(super) fn remove_empty_record_directory(root: &Path, id: &str) {
    let Ok(id) = fmt::id(id) else {
        return;
    };
    let directory = root.join("Records").join(id);
    if io::directory(&directory, false, false) == Ok(true) && fs::remove_dir(&directory).is_ok() {
        let _ = io::sync_parent(&directory);
    }
}

pub(super) fn new_record(root: &Path, id: &str, title: &str) -> Result<PathBuf> {
    fmt::id(id)?;
    let records = root.join("Records");
    if !io::directory(&records, false, false)? {
        return Err("Memory project unavailable".into());
    }
    let directory = records.join(id);
    match fs::create_dir(&directory) {
        Ok(()) => io::sync_parent(&directory)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(io::err(error)),
    }
    if find_record(root, id)?.is_some() {
        return Err("Memory record already exists".into());
    }
    Ok(directory.join(format!("{}.md", fmt::record_basename(title))))
}
