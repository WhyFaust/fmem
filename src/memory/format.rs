use super::{io, Result};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub type Fields = BTreeMap<String, String>;
pub const MEMORY: &str = "fstack_memory: 1";
pub const APPROVED: &str = "fstack_approved: 1";
pub const FORGOTTEN: &str = "fstack_forgotten: 1";
pub fn matches(pattern: &str, value: &str) -> bool {
    thread_local! { static PATTERNS: std::cell::RefCell<BTreeMap<String, Regex>> = const { std::cell::RefCell::new(BTreeMap::new()) }; }
    PATTERNS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= 128 && !cache.contains_key(pattern) {
            cache.clear();
        }
        cache
            .entry(pattern.into())
            .or_insert_with(|| Regex::new(pattern).unwrap())
            .is_match(value)
    })
}
pub fn id(value: &str) -> Result<&str> {
    if matches(r"\A[0-9a-f]{64}\z", value) {
        Ok(value)
    } else {
        Err("Invalid memory entry ID".into())
    }
}
pub fn project(value: &str) -> Result<&str> {
    if !matches(r"\A[a-z0-9][a-z0-9-]{0,47}\z", value)
        || matches(r"\A(?:con|prn|aux|nul|(?:com|lpt)[0-9])\z", value)
    {
        Err("Invalid memory project slug".into())
    } else {
        Ok(value)
    }
}

fn invalid_filename_character(character: char) -> bool {
    character.is_control() || "<>:\"/\\|?*[]#^".contains(character)
}

fn reserved_filename(basename: &str) -> bool {
    let name = basename
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_uppercase();
    matches!(
        name.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        name.strip_prefix(prefix).is_some_and(|suffix| {
            suffix.chars().count() == 1
                && suffix
                    .chars()
                    .all(|character| character.is_ascii_digit() || "¹²³".contains(character))
        })
    })
}

pub(crate) fn validate_record_basename(basename: &str) -> Result<()> {
    if basename.is_empty()
        || basename.starts_with('.')
        || basename.ends_with([' ', '.'])
        || basename.chars().any(invalid_filename_character)
        || reserved_filename(basename)
    {
        return Err("Invalid memory record filename".into());
    }
    Ok(())
}

pub(crate) fn record_basename(title: &str) -> String {
    let normalized = title
        .chars()
        .map(|character| {
            if invalid_filename_character(character) {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut basename = String::new();
    for character in normalized.trim_matches([' ', '.']).chars().take(64) {
        if basename.len() + character.len_utf8() > 120 {
            break;
        }
        basename.push(character);
    }
    let basename = basename.trim_end_matches([' ', '.']);
    if basename.is_empty() {
        "Запись".into()
    } else if reserved_filename(basename) {
        format!("Запись {basename}")
    } else {
        basename.into()
    }
}

fn internal_wikilink(link: &str) -> bool {
    if link.chars().count() > 512
        || link
            .chars()
            .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
    {
        return false;
    }
    let Some(inner) = link
        .strip_prefix("[[")
        .and_then(|link| link.strip_suffix("]]"))
    else {
        return false;
    };
    if inner.contains(['[', ']', '<', '>', '\\']) {
        return false;
    }
    let target = match inner.split_once('|') {
        Some((target, alias)) if !alias.trim().is_empty() && !alias.contains('|') => target,
        Some(_) => return false,
        None => inner,
    };
    !target.trim().is_empty() && !target.contains(':') && !target.trim_start().starts_with("//")
}
pub fn safe(value: &Value, limit: usize, multiline: bool) -> Result<String> {
    let value = value.as_str().ok_or("Invalid memory text")?;
    if value.chars().count() > limit {
        return Err("Invalid memory text length".into());
    }
    let s = value.replace("\r\n", "\n").trim().to_owned();
    if s.is_empty()
        || s.chars().any(|c| {
            ((c as u32) < 32 || (127..=159).contains(&(c as u32)))
                && !(multiline && matches!(c, '\n' | '\t'))
        })
    {
        return Err("Invalid characters in memory text".into());
    }
    let secret = r#"(?i)-----BEGIN [^-\n]*PRIVATE KEY-----|\b(?:sk-[\w-]{16,}|github_pat_[\w]{16,}|gh[pousr]_[\w]{16,}|AKIA[A-Z0-9]{16}|xox[baprs]-[\w-]{12,}|Bearer\s+[\w./+=-]{8,})\b|\b(?:[\w-]*[_-])?(?:password|passwd|secret|api[_-]?key|(?:access|auth)[_-]?token|token)["']?\s*[:=]\s*["']?[^\s"']{4,}|://[^\s/@]+:[^\s/@]+@"#;
    if matches(secret, &s) || matches(r"!\[|</?[a-zA-Z][^>\n]*>|(?m:^\s*(?:```|~~~))", &s) {
        return Err(
            "Memory text contains a possible secret, embedded content or a code block".into(),
        );
    }
    Ok(s)
}
pub fn candidate(v: &Value) -> Result<Fields> {
    let m = v.as_object().ok_or("Invalid memory candidate")?;
    if m.keys().any(|k| {
        !matches!(
            k.as_str(),
            "kind" | "title" | "body" | "source" | "supersedes"
        )
    }) {
        return Err("Invalid memory candidate fields".into());
    }
    let mut out = Fields::new();
    for (k, l) in [
        ("kind", 16),
        ("title", 160),
        ("body", 4000),
        ("source", 300),
    ] {
        if k == "source" && !m.contains_key(k) {
            continue;
        }
        out.insert(
            k.into(),
            safe(m.get(k).unwrap_or(&Value::Null), l, k == "body")?,
        );
    }
    if !matches!(
        out["kind"].as_str(),
        "decision" | "preference" | "correction"
    ) {
        return Err("Invalid memory entry kind".into());
    }
    if let Some(s) = m.get("supersedes") {
        out.insert(
            "supersedes".into(),
            id(s.as_str().ok_or("Invalid predecessor")?)?.into(),
        );
    }
    Ok(out)
}
pub fn markdown(marker: &str, fields: &Fields, body: &str) -> String {
    let metadata = fields
        .iter()
        .map(|(k, v)| format!("{k}: {}", json!(v)))
        .collect::<Vec<_>>()
        .join("\n");
    format!("---\n{marker}\n{metadata}\n---\n\n{body}\n")
}
pub fn parse_record_header(bytes: &[u8], entry: &str) -> Option<Fields> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let mut lines = bytes.split_inclusive(|byte| *byte == b'\n');
    let opening = lines.next()?;
    if !matches!(opening, b"---\n" | b"---\r\n") {
        return None;
    }
    let mut end = opening.len();
    for line in lines {
        end += line.len();
        if matches!(line, b"---\n" | b"---\r\n") {
            let header = std::str::from_utf8(&bytes[..end])
                .ok()?
                .replace("\r\n", "\n");
            return parse(&header, entry, MEMORY, false).ok();
        }
    }
    None
}
pub fn parse(raw: &str, entry: &str, marker: &str, include_body: bool) -> Result<Fields> {
    let (meta, body) = raw
        .strip_prefix("---\n")
        .and_then(|s| s.split_once("\n---\n"))
        .ok_or("Incomplete memory metadata")?;
    if meta.lines().filter(|s| *s == marker).count() != 1 {
        return Err("Invalid memory file marker".into());
    }
    let mut fields = Fields::new();
    let mut decorations = std::collections::BTreeSet::new();
    let mut lines = meta.lines().peekable();
    while let Some(line) = lines.next() {
        if line == marker {
            continue;
        }
        if marker == MEMORY
            && !include_body
            && !matches!(
                line.split(':').next().unwrap_or(""),
                "id" | "fstack_status" | "supersedes"
            )
        {
            continue;
        }
        if marker == MEMORY {
            if let Some((key @ ("tags" | "aliases" | "cssclasses"), _)) = line.split_once(':') {
                if !decorations.insert(key) {
                    return Err("Duplicate Obsidian property".into());
                }
                let mut property = String::from(line);
                while lines.peek().is_some_and(|next| {
                    next.starts_with([' ', '\t'])
                        || next.starts_with("- ")
                        || next.trim().is_empty()
                }) {
                    property.push('\n');
                    property.push_str(lines.next().unwrap());
                }
                let values: BTreeMap<String, serde_yaml::Value> =
                    serde_yaml::from_str(&property).map_err(|_| "Invalid Obsidian property")?;
                let valid = match values.get(key) {
                    Some(serde_yaml::Value::Null | serde_yaml::Value::String(_)) => true,
                    Some(serde_yaml::Value::Sequence(items)) => items.iter().all(|v| v.is_string()),
                    _ => false,
                };
                if values.len() != 1 || !valid {
                    return Err("Invalid Obsidian property".into());
                }
                continue;
            }
        }
        let (k, v) = line.split_once(": ").ok_or("Invalid memory metadata")?;
        let allowed = if marker == MEMORY {
            matches!(
                k,
                "id" | "kind"
                    | "title"
                    | "created"
                    | "source"
                    | "supersedes"
                    | "session_id"
                    | "turn_id"
                    | "fstack_status"
                    | "project_link"
                    | "supersedes_link"
            )
        } else {
            matches!(k, "id" | "supersedes")
        };
        if !allowed || fields.contains_key(k) {
            return Err("Invalid memory metadata".into());
        }
        let value = if v.starts_with('"') {
            serde_json::from_str::<String>(v).map_err(|_| "Invalid memory metadata")?
        } else if v.starts_with('\'') && v.ends_with('\'') {
            if v.len() < 2 {
                return Err("Invalid memory metadata".into());
            }
            v[1..v.len() - 1].replace("''", "'")
        } else {
            v.into()
        };
        fields.insert(k.into(), value);
    }
    if fields.get("id").map(String::as_str) != Some(entry) {
        return Err("Memory entry ID does not match its file".into());
    }
    if let Some(s) = fields.get("supersedes") {
        if id(s)? == entry {
            return Err("Memory entry cannot supersede itself".into());
        }
    }
    if marker == MEMORY {
        if fields.get("fstack_status").is_some_and(|s| s != "approved") {
            return Err("Invalid memory approval status".into());
        }
        for property in ["project_link", "supersedes_link"] {
            if fields
                .get(property)
                .is_some_and(|link| !internal_wikilink(link))
            {
                return Err("Invalid memory graph link".into());
            }
        }
        if include_body {
            safe(&json!(fields.get("title")), 160, false)?;
            if !fields
                .get("kind")
                .is_some_and(|s| matches!(s.as_str(), "decision" | "preference" | "correction"))
            {
                return Err("Invalid memory entry kind".into());
            }
            for (k, v) in &fields {
                if !matches!(
                    k.as_str(),
                    "id" | "title"
                        | "kind"
                        | "supersedes"
                        | "fstack_status"
                        | "project_link"
                        | "supersedes_link"
                ) {
                    safe(&json!(v), 300, false)?;
                }
            }
            fields.insert("body".into(), safe(&json!(body.trim()), 4000, true)?);
        }
    } else if !body.trim().is_empty() {
        return Err("Memory status marker cannot contain text".into());
    }
    Ok(fields)
}
pub fn approval(fields: &Fields) -> Fields {
    fields
        .iter()
        .filter(|(k, _)| matches!(k.as_str(), "id" | "supersedes"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}
pub fn inline(fields: &Fields) -> Option<Fields> {
    (fields.get("fstack_status").map(String::as_str) == Some("approved")).then(|| approval(fields))
}
pub fn public(mut fields: Fields, approval: Option<&Fields>) -> Value {
    for k in ["fstack_status", "project_link", "supersedes_link"] {
        fields.remove(k);
    }
    if let Some(a) = approval {
        fields.remove("supersedes");
        fields.extend(a.clone());
    }
    fields.insert(
        "status".into(),
        if approval.is_some() {
            "memory"
        } else {
            "inbox"
        }
        .into(),
    );
    json!(fields)
}
pub fn stable_id(fields: &Fields) -> String {
    let text = fields
        .iter()
        .filter(|(k, _)| k.as_str() != "source")
        .map(|(k, v)| format!("{}: {}", json!(k), json!(v)))
        .collect::<Vec<_>>()
        .join(", ");
    io::hash(format!("{{{text}}}"))
}
