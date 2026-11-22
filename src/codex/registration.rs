use fmem::{memory::io, protocol, Result};
use serde_json::{json, Value};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
};

pub fn hooks() -> Result<Value> {
    protocol::read_json(include_bytes!("../../hooks/hooks.json").as_slice(), 16_384)
}

pub fn install(package: Option<PathBuf>, codex: &Path, profile: Option<&Path>) -> Result<Value> {
    let package = match package {
        Some(path) => path,
        None => std::env::current_exe()
            .map_err(io::err)?
            .parent()
            .ok_or("Cannot locate the unpacked fmem package")?
            .to_path_buf(),
    };
    let package = io::absolute(&package)?;
    for file in [
        ".codex-plugin/plugin.json",
        ".agents/plugins/marketplace.json",
        "hooks/hooks.json",
        "hooks/memory.sh",
        "hooks/memory.ps1",
        if cfg!(windows) { "fmem.exe" } else { "fmem" },
    ] {
        if !io::file(&package.join(file))? {
            return Err(
                "Use the unpacked fmem release package with its binary and hook files".into(),
            );
        }
    }
    let profile = profile.map(io::absolute).transpose()?;
    let call = |args: &[&OsStr]| -> Result<Value> {
        let mut command = Command::new(codex);
        command.args(args);
        if let Some(path) = &profile {
            command.env("CODEX_HOME", path);
        }
        let output = command
            .output()
            .map_err(|err| format!("Cannot run Codex: {err}"))?;
        if !output.status.success() {
            let reason = String::from_utf8_lossy(&output.stderr);
            let reason = reason.trim().chars().take(1000).collect::<String>();
            return Err(format!("Codex plugin registration failed: {reason}"));
        }
        protocol::read_json(output.stdout.as_slice(), 1_048_576)
    };
    let add_args = ["plugin", "marketplace", "add", "--json"].map(OsStr::new);
    let args = [&add_args[..], &[package.as_os_str()]].concat();
    let marketplace = call(&args)?;
    let plugin = call(&["plugin", "add", "fmem@fmem", "--json"].map(OsStr::new))?;
    Ok(json!({"marketplace":marketplace,"plugin":plugin,"hook_trust":"review_in_codex"}))
}
