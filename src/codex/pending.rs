use super::{format, io, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{
    fs::File,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, PartialEq, Eq)]
pub enum Stage {
    Accepted,
    Conflict,
    Deferred,
    Unavailable,
}

pub struct Grant<'a> {
    pub key: &'a str,
    pub scope: &'a str,
    pub session: &'a str,
    pub turn: &'a str,
    pub delegated: bool,
    pub verified: bool,
}

pub struct Claim {
    pub key: String,
    nonce: String,
    pub turn: String,
    pub data: Value,
}

pub struct Pending {
    state: PathBuf,
    cwd: PathBuf,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn token_parts(token: &str) -> Option<(&str, &str)> {
    let (key, nonce) = token.split_once('.')?;
    (format::id(key).is_ok() && format::matches(r"\A[0-9a-f]{32}\z", nonce)).then_some((key, nonce))
}

impl Pending {
    pub fn new(state: &Path, cwd: &Path) -> Self {
        Self {
            state: state.into(),
            cwd: cwd.into(),
        }
    }
    fn available(&self, create: bool) -> Result<bool> {
        if !io::state(&self.state, &self.cwd, create)? {
            return Ok(false);
        }
        let path = self.state.join("codex-turns.sqlite3");
        if create && io::info(&path)?.is_none() {
            match io::options().write(true).create_new(true).open(&path) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io::err(e)),
            }
        }
        let Some(m) = io::info(&path)? else {
            return Ok(false);
        };
        if !m.is_file() {
            return Err("Unsafe pending memory database".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if m.nlink() != 1 || m.uid() != unsafe { libc::getuid() } || m.mode() & 0o077 != 0 {
                return Err("Unsafe pending memory database".into());
            }
        }
        #[cfg(windows)]
        {
            if io::windows_info(&path, false)?.2 != 1 {
                return Err("Unsafe pending memory database".into());
            }
        }
        if let Some(m) = io::info(&self.state.join("codex-turns.sqlite3-journal"))? {
            if !m.is_file() {
                return Err("Unsafe pending memory journal".into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if m.nlink() != 1 {
                    return Err("Unsafe pending memory journal".into());
                }
            }
        }
        #[cfg(windows)]
        if io::windows_info_if_present(&self.state.join("codex-turns.sqlite3-journal"), false)?
            .is_some_and(|(_, _, links)| links != 1)
        {
            return Err("Unsafe pending memory journal".into());
        }
        for name in ["codex-turns.sqlite3-wal", "codex-turns.sqlite3-shm"] {
            if io::info(&self.state.join(name))?.is_some() {
                return Err("Unexpected pending memory database files".into());
            }
        }
        Ok(true)
    }
    fn connection(&self, create: bool, readonly: bool) -> Result<Option<Connection>> {
        if !self.available(create)? {
            return Ok(None);
        }
        self.open_database(readonly).map(Some).map_err(io::err)
    }

    fn open_database(&self, readonly: bool) -> rusqlite::Result<Connection> {
        let flags = if readonly {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let conn = Connection::open_with_flags(self.state.join("codex-turns.sqlite3"), flags)?;
        conn.busy_timeout(Duration::from_secs(1))?;
        if !readonly {
            conn.execute_batch(
                "PRAGMA secure_delete=ON; PRAGMA synchronous=FULL; BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS turns (
                    key TEXT PRIMARY KEY, nonce TEXT NOT NULL, scope TEXT NOT NULL,
                    session TEXT NOT NULL, turn_id TEXT NOT NULL, expires INTEGER NOT NULL,
                    delegated INTEGER NOT NULL, verified INTEGER NOT NULL,
                    phase TEXT NOT NULL, action TEXT, attempted INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS codex_due ON turns(scope,session,phase,attempted);
                CREATE TABLE IF NOT EXISTS diagnostics (
                    scope TEXT PRIMARY KEY, phase TEXT NOT NULL, updated INTEGER NOT NULL
                );",
            )?;
        }
        Ok(conn)
    }
    fn generation(&self, key: &str) -> Result<Option<String>> {
        if !io::state(&self.state, &self.cwd, false)? {
            return Ok(None);
        }
        io::read(&self.state.join("codex-generations").join(key))
    }
    fn current(&self, key: &str, nonce: &str) -> Result<bool> {
        if token_parts(&format!("{key}.{nonce}")).is_none() {
            return Err("Invalid pending memory generation".into());
        }
        let revoked = self.state.join("codex-revocations");
        Ok(self.generation(key)?.as_deref() == Some(nonce)
            && !(io::directory(&revoked, false, false)?
                && io::info(&revoked.join(format!("{key}.{nonce}")))?.is_some()))
    }
    fn lock_markers(&self) -> Result<std::fs::File> {
        let key = io::digest(&[&io::normalized(&self.state)]);
        io::lock_key(&format!("codex-markers-{}", &key[..2]), None)
    }
    fn prune_markers(&self) -> Result<()> {
        let sweep = self.state.join("codex-marker-sweep");
        let now = SystemTime::now();
        let older_than = |metadata: std::fs::Metadata, seconds| -> Result<bool> {
            Ok(now
                .duration_since(metadata.modified().map_err(io::err)?)
                .unwrap_or_default()
                > Duration::from_secs(seconds))
        };
        if let Some(metadata) = io::info(&sweep)? {
            if !older_than(metadata, 60)? {
                return Ok(());
            }
        }
        for (folder, tokens) in [("codex-generations", false), ("codex-revocations", true)] {
            let dir = self.state.join(folder);
            if !io::directory(&dir, false, false)? {
                continue;
            }
            for entry in std::fs::read_dir(&dir).map_err(io::err)? {
                let entry = entry.map_err(io::err)?;
                let name = entry.file_name();
                let valid = name.to_str().is_some_and(|name| {
                    if tokens {
                        token_parts(name).is_some()
                    } else {
                        format::id(name).is_ok()
                    }
                });
                if valid
                    && entry.file_type().map_err(io::err)?.is_file()
                    && older_than(entry.metadata().map_err(io::err)?, 7200)?
                {
                    io::delete(&entry.path())?;
                }
            }
        }
        io::write(&sweep, "", true, None)?;
        Ok(())
    }
    pub fn invalidate(&self, key: &str) -> Result<Option<String>> {
        if self.generation(key)?.is_none() {
            return Ok(None);
        }
        let _turn = self.lock_turn(key)?;
        self.invalidate_locked(key)
    }

    fn invalidate_locked(&self, key: &str) -> Result<Option<String>> {
        if io::info(&self.state.join("codex-generations").join(key))?.is_none() {
            return Ok(None);
        }
        let _markers = self.lock_markers()?;
        let Some(previous) = self.generation(key)? else {
            return Ok(None);
        };
        if token_parts(&format!("{key}.{previous}")).is_none() {
            return Err("Cannot revoke pending memory action".into());
        }
        let dir = self.state.join("codex-revocations");
        let marker = dir.join(format!("{key}.{previous}"));
        if !io::directory(&dir, false, false)? || io::info(&marker)?.is_none() {
            io::directory(&dir, true, true)?;
            io::write(&marker, "", false, None)?;
            if io::info(&marker)?.is_none() {
                return Err("Missing pending memory revocation marker".into());
            }
        }
        let nonce = io::nonce()?;
        io::write(
            &self.state.join("codex-generations").join(key),
            &nonce,
            true,
            None,
        )?;
        Ok(Some(nonce))
    }
    pub fn cancel(&self, key: &str) -> Result<()> {
        if self.generation(key)?.is_none() {
            return Ok(());
        }
        let _turn = self.lock_turn(key)?;
        if self.invalidate_locked(key)?.is_none() {
            return Ok(());
        }
        let Some(conn) = self.connection(false, false)? else {
            return Ok(());
        };
        let scope: Option<String> = conn
            .query_row("SELECT scope FROM turns WHERE key=?", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(io::err)?;
        if let Some(scope) = scope {
            conn.execute(
                "UPDATE turns SET phase='closed',action=NULL WHERE key=?",
                [key],
            )
            .map_err(io::err)?;
            Self::phase(&conn, &scope, "cancelled").map_err(io::err)?;
        }
        conn.execute_batch("COMMIT").map_err(io::err)
    }

    fn turn_lock_path(&self, key: &str) -> Result<PathBuf> {
        format::id(key)?;
        Ok(self
            .state
            .join("codex-turn-locks")
            .join(format!("{}.lock", &key[..2])))
    }

    pub fn lock_turn(&self, key: &str) -> Result<File> {
        let path = self.turn_lock_path(key)?;
        io::state(&self.state, &self.cwd, true)?;
        io::directory(path.parent().ok_or("Invalid memory lock path")?, true, true)?;
        io::file(&path)?;
        let file = io::options()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .map_err(io::err)?;
        Self::lock_file(file)
    }

    fn lock_file(file: File) -> Result<File> {
        if !file.metadata().map_err(io::err)?.is_file() {
            return Err("Invalid Codex memory turn lock".into());
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(file),
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(_) => return Err("Timed out waiting for Codex memory turn lock".into()),
            }
        }
    }

    pub fn has_generation(&self, key: &str) -> Result<bool> {
        Ok(self.generation(key)?.is_some())
    }

    pub fn authorized_parent(
        &self,
        scope: &str,
        session: &str,
        turn: &str,
        delegated: bool,
    ) -> Result<bool> {
        Ok(self
            .grant_flags(scope, session, turn)?
            .is_some_and(
                |(verified, delegated_grant)| if delegated { delegated_grant } else { verified },
            ))
    }

    pub fn grant_flags(
        &self,
        scope: &str,
        session: &str,
        turn: &str,
    ) -> Result<Option<(bool, bool)>> {
        let key = io::digest(&[session, turn]);
        let Some(nonce) = self.generation(&key)? else {
            return Ok(None);
        };
        if !self.current(&key, &nonce)? {
            return Ok(None);
        }
        let Some(conn) = self.connection(false, true)? else {
            return Ok(None);
        };
        conn.query_row(
            "SELECT verified,delegated FROM turns WHERE key=? AND nonce=? AND scope=?
             AND session=? AND turn_id=? AND expires>=?",
            params![key, nonce, scope, session, turn, now()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(io::err)
    }

    pub fn issue(&self, grant: &Grant<'_>, generation: Option<String>) -> Result<String> {
        let _lock = self.lock_turn(grant.key)?;
        self.capture(grant, generation, "capture")
    }

    pub fn request_recovery(&self, grant: &Grant<'_>) -> Result<Option<(String, bool)>> {
        let key = grant.key;
        let _lock = self.lock_turn(key)?;
        let Some(nonce) = self.generation(key)? else {
            return if grant.delegated && grant.verified {
                self.capture(grant, None, "review")
                    .map(|token| Some((token, grant.delegated)))
            } else {
                Ok(None)
            };
        };
        if !self.current(key, &nonce)? {
            return Ok(None);
        }
        let Some(conn) = self.connection(false, false)? else {
            return Ok(None);
        };
        let delegated = conn
            .query_row(
                "UPDATE turns SET phase='review',verified=1 WHERE key=? AND nonce=?
             AND scope=? AND session=? AND turn_id=? AND expires>=? AND phase='capture'
             RETURNING delegated",
                params![key, nonce, grant.scope, grant.session, grant.turn, now()],
                |row| row.get(0),
            )
            .optional()
            .map_err(io::err)?;
        if delegated.is_some() {
            Self::phase(&conn, grant.scope, "review").map_err(io::err)?;
        }
        conn.execute_batch("COMMIT").map_err(io::err)?;
        Ok(delegated.map(|delegated| (format!("{key}.{nonce}"), delegated)))
    }

    fn capture(
        &self,
        grant: &Grant<'_>,
        generation: Option<String>,
        phase: &str,
    ) -> Result<String> {
        let Grant {
            key,
            scope,
            session,
            turn,
            delegated,
            verified,
        } = *grant;
        let nonce = match generation {
            Some(nonce) => nonce,
            None => {
                io::state(&self.state, &self.cwd, true)?;
                let dir = self.state.join("codex-generations");
                io::directory(&dir, true, true)?;
                let _markers = self.lock_markers()?;
                self.prune_markers()?;
                let nonce = io::nonce()?;
                if !io::write(&dir.join(key), &nonce, false, None)? {
                    return Err("Codex memory turn already exists".into());
                }
                nonce
            }
        };
        let conn = self
            .connection(true, false)?
            .ok_or("Cannot open Codex memory queue")?;
        if !self.current(key, &nonce)? {
            return Err("Codex memory turn was replaced by a newer input".into());
        }
        conn.execute("DELETE FROM turns WHERE expires<?", [now()])
            .map_err(io::err)?;
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM turns WHERE phase!='closed' AND key!=?",
                [key],
                |row| row.get(0),
            )
            .map_err(io::err)?;
        if count >= 256 {
            return Err("Codex memory turn limit exceeded".into());
        }
        conn.execute(
            "INSERT INTO turns (key,nonce,scope,session,turn_id,expires,delegated,verified,phase,action,attempted)
             VALUES (?,?,?,?,?,?,?,?,?,NULL,0) ON CONFLICT(key) DO UPDATE SET
             nonce=excluded.nonce,scope=excluded.scope,session=excluded.session,turn_id=excluded.turn_id,
             expires=excluded.expires,delegated=excluded.delegated,verified=excluded.verified,
             phase=excluded.phase,action=NULL,attempted=0",
            params![key,nonce,scope,session,turn,now()+3600,delegated,verified,phase],
        ).map_err(io::err)?;
        Self::phase(&conn, scope, phase).map_err(io::err)?;
        conn.execute_batch("COMMIT").map_err(io::err)?;
        Ok(format!("{key}.{nonce}"))
    }

    pub fn stage_role(&self, token: &str, scope: &str) -> Result<Option<bool>> {
        let Some((key, nonce)) = token_parts(token) else {
            return Ok(None);
        };
        if !self.current(key, nonce)? {
            return Ok(None);
        }
        let Some(conn) = self.connection(false, true)? else {
            return Ok(None);
        };
        conn.query_row(
            "SELECT delegated FROM turns WHERE key=? AND nonce=? AND scope=? AND expires>=?
             AND phase IN ('capture','review','stage')",
            params![key, nonce, scope, now()],
            |row| row.get(0),
        )
        .optional()
        .map_err(io::err)
    }

    pub fn stage(
        &self,
        token: &str,
        scope: &str,
        action: &Value,
        may_defer: bool,
    ) -> Result<Stage> {
        let Some((key, nonce)) = token_parts(token) else {
            return Ok(Stage::Unavailable);
        };
        if !self.current(key, nonce)? {
            return Ok(Stage::Unavailable);
        }
        let serialized = action.to_string();
        let check = || self.inspect_stage(key, nonce, scope, &serialized);
        if let Some(result) = check()? {
            return Ok(result);
        }
        let path = self.turn_lock_path(key)?;
        if !io::file(&path)? {
            return Ok(Stage::Unavailable);
        }
        let deferred = || check().map(|result| result.unwrap_or(Stage::Deferred));
        let lock = match io::options().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(error)
                if may_defer
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::PermissionDenied
                            | std::io::ErrorKind::ReadOnlyFilesystem
                    ) =>
            {
                return deferred()
            }
            Err(error) => return Err(io::err(error)),
        };
        let _turn = Self::lock_file(lock)?;
        if !self.current(key, nonce)? || !self.available(false)? {
            return Ok(Stage::Unavailable);
        }
        let database = self.state.join("codex-turns.sqlite3");
        match io::options().write(true).open(database) {
            Err(error)
                if may_defer
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::PermissionDenied
                            | std::io::ErrorKind::ReadOnlyFilesystem
                    ) =>
            {
                return deferred()
            }
            Err(error) => return Err(io::err(error)),
            Ok(_) => {}
        }
        let write = || -> rusqlite::Result<bool> {
            let conn = self.open_database(false)?;
            let changed = conn.execute(
                "UPDATE turns SET phase='stage',action=?1 WHERE key=?2 AND nonce=?3 AND scope=?4
                 AND expires>=?5 AND (phase IN ('capture','review') OR (phase='stage' AND action=?1))",
                params![serialized,key,nonce,scope,now()],
            )?;
            if changed > 0 {
                Self::phase(&conn, scope, "stage")?;
            }
            conn.execute_batch("COMMIT")?;
            Ok(changed > 0)
        };
        match write() {
            Ok(true) => Ok(Stage::Accepted),
            Ok(false) => check().map(|result| result.unwrap_or(Stage::Unavailable)),
            Err(error)
                if may_defer
                    && error.sqlite_error_code().is_some_and(|code| {
                        matches!(
                            code,
                            rusqlite::ErrorCode::PermissionDenied | rusqlite::ErrorCode::ReadOnly
                        )
                    }) =>
            {
                deferred()
            }
            Err(error) => Err(io::err(error)),
        }
    }

    fn inspect_stage(
        &self,
        key: &str,
        nonce: &str,
        scope: &str,
        action: &str,
    ) -> Result<Option<Stage>> {
        if !self.current(key, nonce)? {
            return Ok(Some(Stage::Unavailable));
        }
        let Some(conn) = self.connection(false, true)? else {
            return Ok(Some(Stage::Unavailable));
        };
        let existing: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT phase,action FROM turns WHERE key=? AND nonce=? AND scope=? AND expires>=?",
                params![key, nonce, scope, now()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(io::err)?;
        Ok(match existing {
            Some((phase, _)) if matches!(phase.as_str(), "capture" | "review") => None,
            Some((phase, stored)) if phase == "stage" => {
                Some(if stored.as_deref() == Some(action) {
                    Stage::Accepted
                } else {
                    Stage::Conflict
                })
            }
            _ => Some(Stage::Unavailable),
        })
    }

    pub fn claim_current(&self, grant: &Grant<'_>, first_stop: bool) -> Result<()> {
        let _lock = self.lock_turn(grant.key)?;
        let Some(nonce) = self.generation(grant.key)? else {
            return Ok(());
        };
        if !self.current(grant.key, &nonce)? {
            return Ok(());
        }
        let Some(conn) = self.connection(false, false)? else {
            return Ok(());
        };
        let phase: Option<String> = conn
            .query_row(
                "SELECT phase FROM turns WHERE key=? AND nonce=? AND scope=?
             AND session=? AND turn_id=? AND expires>=?",
                params![
                    grant.key,
                    nonce,
                    grant.scope,
                    grant.session,
                    grant.turn,
                    now()
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(io::err)?;
        let next = match phase.as_deref() {
            Some("stage") => Some(("pending", "pending")),
            Some("capture") => Some(("closed", "skipped")),
            Some("review") if !first_stop => Some(("closed", "skipped")),
            _ => None,
        };
        if let Some((phase, observed)) = next {
            conn.execute(
                "UPDATE turns SET phase=?,verified=1 WHERE key=?",
                params![phase, grant.key],
            )
            .map_err(io::err)?;
            Self::phase(&conn, grant.scope, observed).map_err(io::err)?;
        }
        conn.execute_batch("COMMIT").map_err(io::err)
    }

    pub fn due(&self, scope: &str, session: &str, current: &str) -> Result<Vec<String>> {
        let Some(conn) = self.connection(false, true)? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn
            .prepare(
                "SELECT key,nonce FROM turns WHERE scope=?1 AND session=?2 AND expires>=?3
             AND (phase='pending' OR (phase='stage' AND verified=1 AND key!=?4))
             ORDER BY key=?4 DESC,attempted,expires,key LIMIT 8",
            )
            .map_err(io::err)?;
        let entries = stmt
            .query_map(params![scope, session, now(), current], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(io::err)?;
        let mut due = Vec::new();
        for entry in entries {
            let (key, nonce) = entry.map_err(io::err)?;
            if self.current(&key, &nonce)? {
                due.push(key);
                if due.len() == 2 {
                    break;
                }
            }
        }
        Ok(due)
    }

    pub fn operation(
        &self,
        key: &str,
        scope: &str,
        session: &str,
        replay: bool,
    ) -> Result<Option<Claim>> {
        let Some(conn) = self.connection(false, false)? else {
            return Ok(None);
        };
        let row: Option<(String, String, String)> = conn.query_row(
            "SELECT nonce,turn_id,action FROM turns WHERE key=? AND scope=? AND session=? AND expires>=?
             AND (phase='pending' OR (? AND phase='stage' AND verified=1))",
            params![key,scope,session,now(),replay],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).optional().map_err(io::err)?;
        let Some((nonce, turn, action)) = row else {
            return Ok(None);
        };
        if !self.current(key, &nonce)? {
            return Ok(None);
        }
        conn.execute("UPDATE turns SET phase='pending' WHERE key=?", [key])
            .map_err(io::err)?;
        conn.execute_batch("COMMIT").map_err(io::err)?;
        Ok(Some(Claim {
            key: key.into(),
            nonce,
            turn,
            data: serde_json::from_str(&action).map_err(io::err)?,
        }))
    }

    pub fn finish(&self, scope: &str, claim: &Claim, phase: &str, complete: bool) -> Result<()> {
        let conn = self
            .connection(false, false)?
            .ok_or("Cannot open Codex memory queue")?;
        conn.execute(
            "UPDATE turns SET attempted=?1,phase=CASE WHEN ?2 THEN 'closed' ELSE phase END,
             action=CASE WHEN ?2 THEN NULL ELSE action END WHERE key=?3 AND nonce=?4",
            params![now(), complete, claim.key, claim.nonce],
        )
        .map_err(io::err)?;
        Self::phase(&conn, scope, phase).map_err(io::err)?;
        conn.execute_batch("COMMIT").map_err(io::err)
    }

    fn phase(conn: &Connection, scope: &str, phase: &str) -> rusqlite::Result<()> {
        conn.execute("DELETE FROM diagnostics WHERE updated<?", [now() - 3600])?;
        conn.execute(
            "DELETE FROM diagnostics WHERE scope IN
             (SELECT scope FROM diagnostics ORDER BY updated DESC,scope LIMIT -1 OFFSET 255)",
            [],
        )?;
        conn.execute(
            "INSERT INTO diagnostics VALUES (?,?,?) ON CONFLICT(scope)
             DO UPDATE SET phase=excluded.phase,updated=excluded.updated",
            params![scope, phase, now()],
        )?;
        Ok(())
    }

    pub fn diagnostics(&self, scope: &str) -> Result<Value> {
        let empty = json!({"phase":null,"staged":0,"pending":0});
        let Some(conn) = self.connection(false, true)? else {
            return Ok(empty);
        };
        let initialized: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='diagnostics')",
                [],
                |row| row.get(0),
            )
            .map_err(io::err)?;
        if !initialized {
            return Ok(empty);
        }
        let phase: Option<String> = conn
            .query_row(
                "SELECT phase FROM diagnostics WHERE scope=? AND updated>=?",
                params![scope, now() - 3600],
                |row| row.get(0),
            )
            .optional()
            .map_err(io::err)?;
        let mut stmt = conn.prepare("SELECT key,nonce,phase FROM turns WHERE scope=? AND expires>=? AND phase IN ('stage','pending')").map_err(io::err)?;
        let rows = stmt
            .query_map(params![scope, now()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(io::err)?;
        let mut staged = 0;
        let mut pending = 0;
        for row in rows {
            let (key, nonce, phase) = row.map_err(io::err)?;
            if self.current(&key, &nonce)? {
                if phase == "stage" {
                    staged += 1;
                } else {
                    pending += 1;
                }
            }
        }
        Ok(json!({"phase":phase,"staged":staged,"pending":pending}))
    }
}
