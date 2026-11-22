use super::{format, io, read_json};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Component, Path},
};

const HEADER_LIMIT: u64 = 256 * 1024;
const TAIL_LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Default,
    Plan,
}

impl Mode {
    fn read(value: &Value) -> Option<Self> {
        match value.as_str()? {
            "default" => Some(Self::Default),
            "plan" => Some(Self::Plan),
            _ => None,
        }
    }
}

pub(super) struct Turn {
    pub mode: Mode,
    pub delegated: bool,
    pub parent_turn: Option<String>,
    pub root_turn: Option<String>,
    pub parent_session: Option<String>,
}

pub(super) struct CancellationAncestry {
    pub parent_session: String,
    pub parent_turn: String,
    pub root_turn: String,
}

struct Transcript {
    header: Value,
    tail: String,
}

fn turn_id(id: &Value) -> bool {
    id.as_str()
        .is_some_and(|id| format::matches(r"\A[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}\z", id))
}

fn session_file(profile: &Path, transcript: &Path, session: &str) -> Option<()> {
    let sessions = profile.join("sessions");
    if !transcript.is_absolute()
        || transcript
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        || !io::same_directory(transcript.ancestors().nth(4)?, &sessions)
    {
        return None;
    }
    let file = transcript.file_name()?.to_str()?;
    let time = file
        .strip_prefix("rollout-")?
        .strip_suffix(".jsonl")?
        .strip_suffix(session)?;
    if !format::matches(
        r"\A[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}-[0-9]{2}-[0-9]{2}-\z",
        time,
    ) {
        return None;
    }
    let days = [(&time[0..4], 3), (&time[5..7], 2), (&time[8..10], 1)];
    days.into_iter()
        .all(|(date, up)| {
            transcript
                .ancestors()
                .nth(up)
                .and_then(Path::file_name)
                .is_some_and(|part| part == date)
        })
        .then_some(())
}

fn transcript(profile: &Path, path: &Value, session: &str) -> Option<Transcript> {
    let path = Path::new(path.as_str()?);
    session_file(profile, path, session)?;
    if !io::file(path).ok()? {
        return None;
    }
    #[cfg(windows)]
    if io::windows_info(path, false).ok()?.2 != 1 {
        return None;
    }
    let file = io::options().read(true).open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 || metadata.uid() != unsafe { libc::getuid() } {
            return None;
        }
    }
    let mut file = BufReader::new(file);
    let mut first = Vec::new();
    (&mut file)
        .take(HEADER_LIMIT)
        .read_until(b'\n', &mut first)
        .ok()?;
    if first.last() != Some(&b'\n') {
        return None;
    }
    let mut meta = read_json(first.as_slice(), first.len()).ok()?;
    if meta["type"] != "session_meta" || meta["payload"]["id"] != session {
        return None;
    }

    let head = first.len() as u64;
    let start = metadata.len().saturating_sub(TAIL_LIMIT).max(head);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::new();
    file.take(TAIL_LIMIT).read_to_end(&mut tail).ok()?;
    let first_line = if start == head {
        0
    } else {
        tail.iter().position(|&c| c == b'\n')? + 1
    };
    let end = tail.iter().rposition(|&c| c == b'\n')?;
    if first_line > end {
        return None;
    }
    tail.truncate(end);
    tail.drain(..first_line);
    Some(Transcript {
        header: meta["payload"].take(),
        tail: String::from_utf8(tail).ok()?,
    })
}

fn app_task(data: &Value, turn: &Value) -> bool {
    let attribution = &data["turn_attribution"];
    attribution["turn_id"] == *turn
        && matches!(
            attribution["turn_trigger"].as_str(),
            Some("app_tool_create_thread" | "app_tool_send_message")
        )
        && turn_id(&attribution["parent_turn_id"])
        && turn_id(&attribution["root_turn_id"])
        && attribution["parent_turn_id"] != *turn
        && data["root_turn_id"] == attribution["root_turn_id"]
}

struct ActiveTurn {
    id: String,
    mode: Mode,
    context_seen: bool,
    delegated: bool,
    parent_turn: Option<String>,
    root_turn: Option<String>,
}

fn latest_turn(
    transcript: &Transcript,
    cwd: Option<&Path>,
    expected: Option<&str>,
    allow_completed: bool,
) -> Option<Turn> {
    let mut turn: Option<ActiveTurn> = None;
    let mut communication = false;
    for line in transcript.tail.lines().filter(|line| {
        [
            "\"task_started\"",
            "\"turn_started\"",
            "\"task_complete\"",
            "\"turn_complete\"",
            "\"turn_aborted\"",
            "\"turn_context\"",
            "\"inter_agent_communication_metadata\"",
            "\"agent_message\"",
        ]
        .iter()
        .any(|kind| line.contains(kind))
    }) {
        let row = read_json(line.as_bytes(), line.len()).ok()?;
        let data = &row["payload"];
        let kind = row["type"].as_str();
        let event = data["type"].as_str();
        if kind == Some("event_msg") && matches!(event, Some("task_started" | "turn_started")) {
            turn = data["turn_id"].as_str().and_then(|id| {
                let mode = Mode::read(&data["collaboration_mode_kind"])?;
                let attribution = &data["turn_attribution"];
                let parent = &attribution["parent_turn_id"];
                let parent_turn = parent
                    .as_str()
                    .filter(|_| attribution["turn_id"] == id && turn_id(parent))
                    .map(str::to_owned);
                let root = &attribution["root_turn_id"];
                let root_turn = root
                    .as_str()
                    .filter(|_| {
                        attribution["turn_id"] == id
                            && data["root_turn_id"] == *root
                            && turn_id(root)
                    })
                    .map(str::to_owned);
                expected
                    .is_none_or(|expected| expected == id)
                    .then(|| ActiveTurn {
                        id: id.to_owned(),
                        mode,
                        context_seen: false,
                        delegated: app_task(data, &data["turn_id"]),
                        parent_turn,
                        root_turn,
                    })
            });
            communication = false;
            continue;
        }
        let Some(active) = turn.as_mut() else {
            continue;
        };
        match (kind, event) {
            (Some("event_msg"), Some("task_complete" | "turn_complete" | "turn_aborted"))
                if data["turn_id"] == active.id =>
            {
                if !allow_completed || event == Some("turn_aborted") {
                    turn = None;
                }
            }
            (Some("turn_context"), _) if data["turn_id"] == active.id => {
                if let Some(cwd) = cwd {
                    let recorded = Path::new(data["cwd"].as_str()?);
                    if !io::same_directory(recorded, cwd) {
                        return None;
                    }
                }
                let mode = Mode::read(&data["collaboration_mode"]["mode"])?;
                if mode == Mode::Plan {
                    active.mode = mode;
                }
                active.context_seen = true;
            }
            (Some("inter_agent_communication_metadata"), _) => {
                communication = data["trigger_turn"].is_boolean();
            }
            (Some("response_item"), Some("agent_message")) => {
                active.delegated |= communication
                    && data["recipient"] == "/root"
                    && data["author"]
                        .as_str()
                        .is_some_and(|author| author.starts_with("/root/"));
                communication = false;
            }
            _ => {}
        }
    }
    let turn = turn?;
    (turn.context_seen || turn.mode == Mode::Plan).then_some(Turn {
        mode: turn.mode,
        delegated: turn.delegated,
        parent_turn: turn.parent_turn,
        root_turn: turn.root_turn,
        parent_session: None,
    })
}

fn hook_transcript(profile: &Path, payload: &Value, session: &str) -> Option<Transcript> {
    let path = if payload["hook_event_name"] == "SubagentStop" {
        &payload["agent_transcript_path"]
    } else {
        &payload["transcript_path"]
    };
    transcript(profile, path, session)
}

pub(super) fn current(
    profile: &Path,
    payload: &Value,
    cwd: &Path,
    actor: &str,
    root_session: Option<&str>,
    turn: Option<&str>,
) -> Option<Turn> {
    let local = hook_transcript(profile, payload, actor)?;
    let mut turn = latest_turn(&local, Some(cwd), turn, false)?;
    let source = &local.header["source"];
    if let Some(root_session) = root_session {
        let parent = parent_session(&local)?;
        let parent_turn = turn.parent_turn.as_deref()?;
        let root_turn = turn.root_turn.as_deref()?;
        if parent == actor || (parent == root_session) != (parent_turn == root_turn) {
            return None;
        }
        turn.delegated = true;
        if payload["hook_event_name"] == "SubagentStop" {
            let parent = transcript(profile, &payload["transcript_path"], parent)?;
            let parent = latest_turn(&parent, Some(cwd), Some(parent_turn), true)?;
            if parent.mode == Mode::Plan {
                turn.mode = Mode::Plan;
            }
        }
        turn.parent_session = Some(parent.to_owned());
    } else {
        if source.get("subagent").is_some() {
            return None;
        }
        turn.delegated &= local.header["originator"] == "Codex Desktop" && source == "vscode";
    }
    Some(turn)
}

fn parent_session(transcript: &Transcript) -> Option<&str> {
    let parent = &transcript.header["source"]["subagent"]["thread_spawn"]["parent_thread_id"];
    parent.as_str().filter(|_| turn_id(parent))
}

pub(super) fn child_of(profile: &Path, payload: &Value, actor: &str, session: &str) -> bool {
    hook_transcript(profile, payload, actor)
        .as_ref()
        .and_then(parent_session)
        == Some(session)
}

pub(super) fn cancellation_ancestry(
    profile: &Path,
    payload: &Value,
    cwd: &Path,
    actor: &str,
    root_session: &str,
    turn: &str,
) -> Option<CancellationAncestry> {
    let local = hook_transcript(profile, payload, actor)?;
    let parent = parent_session(&local)?;
    if parent == actor {
        return None;
    }
    let mut ancestry = None;
    let mut context_seen = false;
    for line in local.tail.lines().filter(|line| {
        ["\"task_started\"", "\"turn_started\"", "\"turn_context\""]
            .iter()
            .any(|kind| line.contains(kind))
    }) {
        let row = read_json(line.as_bytes(), line.len()).ok()?;
        let data = &row["payload"];
        if data["turn_id"] != turn {
            continue;
        }
        match (row["type"].as_str(), data["type"].as_str()) {
            (Some("event_msg"), Some("task_started" | "turn_started")) => {
                let attribution = &data["turn_attribution"];
                let parent_turn = &attribution["parent_turn_id"];
                let root_turn = &attribution["root_turn_id"];
                if attribution["turn_id"] != turn
                    || !turn_id(parent_turn)
                    || !turn_id(root_turn)
                    || data["root_turn_id"] != *root_turn
                    || (parent == root_session) != (parent_turn == root_turn)
                {
                    return None;
                }
                ancestry = Some(CancellationAncestry {
                    parent_session: parent.to_owned(),
                    parent_turn: parent_turn.as_str()?.to_owned(),
                    root_turn: root_turn.as_str()?.to_owned(),
                });
                context_seen = false;
            }
            (Some("turn_context"), _) if ancestry.is_some() => {
                if !io::same_directory(Path::new(data["cwd"].as_str()?), cwd) {
                    return None;
                }
                context_seen = true;
            }
            _ => {}
        }
    }
    context_seen.then_some(ancestry?)
}

pub(super) fn cancelled(profile: &Path, payload: &Value, session: &str, turn: &str) -> bool {
    let Some(transcript) = hook_transcript(profile, payload, session) else {
        return false;
    };
    transcript
        .tail
        .lines()
        .filter(|line| {
            line.contains(turn) && (line.contains("\"turn_aborted\"") || line.contains("\"plan\""))
        })
        .filter_map(|line| read_json(line.as_bytes(), line.len()).ok())
        .any(|row| {
            let data = &row["payload"];
            data["turn_id"] == turn
                && match row["type"].as_str() {
                    Some("event_msg") => {
                        data["type"] == "turn_aborted"
                            || (matches!(
                                data["type"].as_str(),
                                Some("task_started" | "turn_started")
                            ) && data["collaboration_mode_kind"] == "plan")
                    }
                    Some("turn_context") => data["collaboration_mode"]["mode"] == "plan",
                    _ => false,
                }
        })
}
