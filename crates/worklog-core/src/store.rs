//! SQLite 저장소 (`~/.worklog/worklog.db`).
//!
//! 테이블: notes(메모) · runs(생성 실행 이력) · documents(생성된 일지) · repos(저장소 캐시) ·
//! file_state(jsonl 증분 파싱 위치) · kv(잡동사니). 마크다운 파일은 여전히 내보내기 결과이고,
//! 앱 화면은 이 DB 를 읽는다. 시각은 RFC3339(UTC, 밀리초) 문자열, 날짜는 "YYYY-MM-DD" 로 저장한다 —
//! 같은 형식이라 문자열 비교가 시간 순서와 일치한다.

use std::path::Path;

use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::time::now_rfc3339;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("잘못된 저장 값: {0}")]
    BadValue(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub const SCHEMA_VERSION: i32 = 1;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS notes (
  id        INTEGER PRIMARY KEY,
  date      TEXT    NOT NULL,
  ts        TEXT    NOT NULL,
  text      TEXT    NOT NULL,
  tags      TEXT    NOT NULL DEFAULT '[]',
  mentions  TEXT    NOT NULL DEFAULT '[]',
  source    TEXT    NOT NULL DEFAULT 'app',
  created   TEXT    NOT NULL,
  updated   TEXT    NOT NULL,
  deleted   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS notes_date ON notes(date, deleted, ts);

CREATE TABLE IF NOT EXISTS runs (
  id          INTEGER PRIMARY KEY,
  date        TEXT    NOT NULL,
  kind        TEXT    NOT NULL,
  started     TEXT    NOT NULL,
  finished    TEXT,
  status      TEXT    NOT NULL,
  error       TEXT,
  duration_ms INTEGER
);
CREATE INDEX IF NOT EXISTS runs_started ON runs(started DESC);

CREATE TABLE IF NOT EXISTS documents (
  date         TEXT PRIMARY KEY,
  summary_md   TEXT,
  full_md      TEXT NOT NULL,
  generated_at TEXT NOT NULL,
  edited_at    TEXT,
  run_id       INTEGER
);

CREATE TABLE IF NOT EXISTS repos (
  common_dir     TEXT PRIMARY KEY,
  path           TEXT NOT NULL,
  name           TEXT NOT NULL,
  source         TEXT NOT NULL,
  first_seen     TEXT NOT NULL,
  last_seen      TEXT NOT NULL,
  last_commit_ts TEXT
);

CREATE TABLE IF NOT EXISTS file_state (
  path       TEXT PRIMARY KEY,
  offset     INTEGER NOT NULL,
  size       INTEGER NOT NULL,
  mtime_ns   INTEGER NOT NULL,
  session_id TEXT
);

CREATE TABLE IF NOT EXISTS kv (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#;

// --------------------------------------------------------------------------- //
// 행 타입
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: i64,
    pub date: NaiveDate,
    pub ts: DateTime<Utc>,
    pub text: String,
    pub tags: Vec<String>,
    pub mentions: Vec<String>,
    /// app | tray | cli
    pub source: String,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub deleted: bool,
}

#[derive(Debug, Clone)]
pub struct NewNote<'a> {
    pub date: NaiveDate,
    pub ts: DateTime<Utc>,
    pub text: &'a str,
    pub tags: &'a [String],
    pub mentions: &'a [String],
    pub source: &'a str,
}

pub mod run_kind {
    pub const MANUAL: &str = "manual";
    pub const AUTO: &str = "auto";
    pub const RETRY: &str = "retry";
}

pub mod run_status {
    pub const RUNNING: &str = "running";
    pub const OK: &str = "ok";
    pub const FAILED: &str = "failed";
    pub const CANCELLED: &str = "cancelled";
    /// 앱이 죽어 끝을 못 본 실행(재시작 시 정리).
    pub const ABANDONED: &str = "abandoned";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: i64,
    pub date: NaiveDate,
    pub kind: String,
    pub started: DateTime<Utc>,
    pub finished: Option<DateTime<Utc>>,
    pub status: String,
    pub error: Option<String>,
    /// 실제 소요 시간. 비정상 종료 정리(`run_abandon`)에는 없음.
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub date: NaiveDate,
    pub summary_md: Option<String>,
    pub full_md: String,
    pub generated_at: DateTime<Utc>,
    /// 사용자가 편집한 시각. `document_put` 은 무시하고 항상 지운다(새로 생성했으므로).
    pub edited_at: Option<DateTime<Utc>>,
    pub run_id: Option<i64>,
}

impl Document {
    pub fn is_edited(&self) -> bool {
        self.edited_at.is_some()
    }
}

/// 달력 표시용: 날짜별 생성/편집 여부.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocStatus {
    pub date: NaiveDate,
    pub edited: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoEntry {
    /// git-common-dir(realpath). 물리 저장소 식별 키.
    pub common_dir: String,
    /// 작업 트리 경로(첫 발견 경로).
    pub path: String,
    pub name: String,
    /// config | scan | claude
    pub source: String,
    /// 처음 캐시에 들어온 시각. INSERT 때만 기록되고 이후 갱신되지 않는다.
    pub first_seen: DateTime<Utc>,
    /// 마지막으로 탐색에서 본 시각(스캔마다 갱신).
    pub last_seen: DateTime<Utc>,
    pub last_commit_ts: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileState {
    pub path: String,
    /// 다음에 읽기 시작할 바이트 오프셋.
    pub offset: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub session_id: Option<String>,
}

// --------------------------------------------------------------------------- //
// 변환 헬퍼
// --------------------------------------------------------------------------- //

fn date_str(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

fn parse_date(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| StoreError::BadValue(format!("date {s:?}")))
}

fn ts_str(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|_| StoreError::BadValue(format!("timestamp {s:?}")))
}

fn parse_ts_opt(s: Option<String>) -> Result<Option<DateTime<Utc>>> {
    s.map(|s| parse_ts(&s)).transpose()
}

fn to_json(v: &[String]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}

fn from_json(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

fn map_bad(e: StoreError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

// --------------------------------------------------------------------------- //
// Store
// --------------------------------------------------------------------------- //

pub struct Store {
    conn: Connection,
}

impl Store {
    /// 파일 DB 를 연다(없으면 생성). WAL 모드, 스키마 마이그레이션 수행.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        let s = Self { conn };
        s.migrate()?;
        Ok(s)
    }

    /// 테스트용 인메모리 DB.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        let s = Self { conn };
        s.migrate()?;
        Ok(s)
    }

    fn migrate(&self) -> Result<()> {
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            self.conn.execute_batch(SCHEMA_V1)?;
        }
        if version < SCHEMA_VERSION {
            self.conn
                .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i32> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    // ---- notes ---------------------------------------------------------- //

    pub fn note_add(&self, n: &NewNote<'_>) -> Result<Note> {
        let now = now_rfc3339();
        self.conn.execute(
            "INSERT INTO notes(date, ts, text, tags, mentions, source, created, updated, deleted)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, 0)",
            params![
                date_str(n.date),
                ts_str(&n.ts),
                n.text,
                to_json(n.tags),
                to_json(n.mentions),
                n.source,
                now
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.note_get(id)?
            .ok_or_else(|| StoreError::BadValue(format!("note {id} 삽입 직후 조회 실패")))
    }

    pub fn note_get(&self, id: i64) -> Result<Option<Note>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, date, ts, text, tags, mentions, source, created, updated, deleted
                 FROM notes WHERE id = ?1",
                params![id],
                row_to_note,
            )
            .optional()?)
    }

    /// 그날의 살아있는 메모(시간순).
    pub fn notes_for(&self, date: NaiveDate) -> Result<Vec<Note>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, date, ts, text, tags, mentions, source, created, updated, deleted
             FROM notes WHERE date = ?1 AND deleted = 0 ORDER BY ts, id",
        )?;
        let rows = stmt.query_map(params![date_str(date)], row_to_note)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// 본문·태그·멘션 갱신. 삭제된 메모나 없는 id 면 false.
    pub fn note_update(
        &self,
        id: i64,
        text: &str,
        tags: &[String],
        mentions: &[String],
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE notes SET text = ?2, tags = ?3, mentions = ?4, updated = ?5
             WHERE id = ?1 AND deleted = 0",
            params![id, text, to_json(tags), to_json(mentions), now_rfc3339()],
        )?;
        Ok(n == 1)
    }

    /// 소프트 삭제. 이미 삭제됐거나 없으면 false.
    pub fn note_delete(&self, id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE notes SET deleted = 1, updated = ?2 WHERE id = ?1 AND deleted = 0",
            params![id, now_rfc3339()],
        )?;
        Ok(n == 1)
    }

    /// 메모가 있는 날짜들(살아있는 것만, 최신순). 검색·달력 표시용.
    pub fn note_dates(&self, limit: usize) -> Result<Vec<NaiveDate>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT date FROM notes WHERE deleted = 0 ORDER BY date DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(parse_date(&r?)?);
        }
        Ok(out)
    }

    // ---- runs ----------------------------------------------------------- //

    /// 실행 시작 기록 → run id.
    pub fn run_start(&self, date: NaiveDate, kind: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO runs(date, kind, started, status) VALUES (?1, ?2, ?3, ?4)",
            params![date_str(date), kind, now_rfc3339(), run_status::RUNNING],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 정상 종료 기록(상태·오류·실제 소요 시간).
    pub fn run_finish(&self, id: i64, status: &str, error: Option<&str>) -> Result<()> {
        let started: String =
            self.conn
                .query_row("SELECT started FROM runs WHERE id = ?1", params![id], |r| {
                    r.get(0)
                })?;
        let now = Utc::now();
        let dur = (now - parse_ts(&started)?).num_milliseconds().max(0);
        self.conn.execute(
            "UPDATE runs SET finished = ?2, status = ?3, error = ?4, duration_ms = ?5 WHERE id = ?1",
            params![id, ts_str(&now), status, error, dur],
        )?;
        Ok(())
    }

    /// 앱이 죽어 끝을 못 본 실행을 정리한다. 소요 시간은 알 수 없으므로 기록하지 않는다.
    pub fn run_abandon(&self, id: i64, error: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET finished = ?2, status = ?3, error = ?4, duration_ms = NULL
             WHERE id = ?1 AND status = ?5",
            params![
                id,
                now_rfc3339(),
                run_status::ABANDONED,
                error,
                run_status::RUNNING
            ],
        )?;
        Ok(())
    }

    pub fn run_get(&self, id: i64) -> Result<Option<Run>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, date, kind, started, finished, status, error, duration_ms FROM runs WHERE id = ?1",
                params![id],
                row_to_run,
            )
            .optional()?)
    }

    /// 최근 실행(최신순).
    pub fn runs_recent(&self, limit: usize) -> Result<Vec<Run>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, date, kind, started, finished, status, error, duration_ms
             FROM runs ORDER BY started DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_run)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// 아직 `running` 인 실행들(앱이 죽었다 살아난 뒤 정리용).
    pub fn runs_running(&self) -> Result<Vec<Run>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, date, kind, started, finished, status, error, duration_ms
             FROM runs WHERE status = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![run_status::RUNNING], row_to_run)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ---- documents ------------------------------------------------------ //

    /// 생성 결과 저장(같은 날짜면 교체). 편집 표식(`edited_at`)은 입력과 무관하게 항상 지운다.
    pub fn document_put(&self, d: &Document) -> Result<()> {
        self.conn.execute(
            "INSERT INTO documents(date, summary_md, full_md, generated_at, edited_at, run_id)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5)
             ON CONFLICT(date) DO UPDATE SET
               summary_md = excluded.summary_md, full_md = excluded.full_md,
               generated_at = excluded.generated_at, edited_at = NULL,
               run_id = excluded.run_id",
            params![
                date_str(d.date),
                d.summary_md,
                d.full_md,
                ts_str(&d.generated_at),
                d.run_id
            ],
        )?;
        Ok(())
    }

    pub fn document_get(&self, date: NaiveDate) -> Result<Option<Document>> {
        Ok(self
            .conn
            .query_row(
                "SELECT date, summary_md, full_md, generated_at, edited_at, run_id
                 FROM documents WHERE date = ?1",
                params![date_str(date)],
                row_to_document,
            )
            .optional()?)
    }

    /// 사용자가 편집한 본문 저장. `summary_md` 가 None 이면 기존 요약 유지. 문서가 없으면 false.
    pub fn document_mark_edited(
        &self,
        date: NaiveDate,
        full_md: &str,
        summary_md: Option<&str>,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE documents SET full_md = ?2, summary_md = COALESCE(?3, summary_md), edited_at = ?4
             WHERE date = ?1",
            params![date_str(date), full_md, summary_md, now_rfc3339()],
        )?;
        Ok(n == 1)
    }

    pub fn document_delete(&self, date: NaiveDate) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM documents WHERE date = ?1",
            params![date_str(date)],
        )?;
        Ok(n == 1)
    }

    /// `[from, to]` 구간(양끝 포함)의 문서 상태(달력용, 날짜 오름차순).
    pub fn document_statuses(&self, from: NaiveDate, to: NaiveDate) -> Result<Vec<DocStatus>> {
        let mut stmt = self.conn.prepare(
            "SELECT date, edited_at IS NOT NULL FROM documents
             WHERE date >= ?1 AND date <= ?2 ORDER BY date",
        )?;
        let rows = stmt.query_map(params![date_str(from), date_str(to)], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (d, edited) = r?;
            out.push(DocStatus {
                date: parse_date(&d)?,
                edited,
            });
        }
        Ok(out)
    }

    /// 최근 문서 날짜(최신순).
    pub fn document_dates(&self, limit: usize) -> Result<Vec<NaiveDate>> {
        let mut stmt = self
            .conn
            .prepare("SELECT date FROM documents ORDER BY date DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit as i64], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(parse_date(&r?)?);
        }
        Ok(out)
    }

    // ---- repos ---------------------------------------------------------- //

    /// 저장소 캐시 갱신. 이미 있으면 path/name/source/last_seen 을 갱신하고, `first_seen` 은 유지하며,
    /// `last_commit_ts` 는 더 최신 값만 반영한다(None 이면 기존 유지).
    pub fn repo_upsert(&self, r: &RepoEntry) -> Result<()> {
        self.conn.execute(
            "INSERT INTO repos(common_dir, path, name, source, first_seen, last_seen, last_commit_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(common_dir) DO UPDATE SET
               path = excluded.path, name = excluded.name, source = excluded.source,
               last_seen = excluded.last_seen,
               last_commit_ts = CASE
                 WHEN excluded.last_commit_ts IS NULL THEN repos.last_commit_ts
                 WHEN repos.last_commit_ts IS NULL THEN excluded.last_commit_ts
                 WHEN excluded.last_commit_ts > repos.last_commit_ts THEN excluded.last_commit_ts
                 ELSE repos.last_commit_ts END",
            params![
                r.common_dir,
                r.path,
                r.name,
                r.source,
                ts_str(&r.first_seen),
                ts_str(&r.last_seen),
                r.last_commit_ts.as_ref().map(ts_str)
            ],
        )?;
        Ok(())
    }

    pub fn repos_all(&self) -> Result<Vec<RepoEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT common_dir, path, name, source, first_seen, last_seen, last_commit_ts
             FROM repos ORDER BY name, common_dir",
        )?;
        let rows = stmt.query_map([], row_to_repo)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// `since` 이후 커밋이 있었거나, `since` 이후 처음 발견된 저장소(감시 대상 선별용).
    /// 매 스캔마다 갱신되는 `last_seen` 은 보지 않는다 — 그러면 전체 스캔 직후 모든 저장소가 걸린다.
    pub fn repos_active_since(&self, since: &DateTime<Utc>) -> Result<Vec<RepoEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT common_dir, path, name, source, first_seen, last_seen, last_commit_ts FROM repos
             WHERE (last_commit_ts IS NOT NULL AND last_commit_ts >= ?1) OR first_seen >= ?1
             ORDER BY name, common_dir",
        )?;
        let rows = stmt.query_map(params![ts_str(since)], row_to_repo)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn repo_delete(&self, common_dir: &str) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM repos WHERE common_dir = ?1",
            params![common_dir],
        )?;
        Ok(n == 1)
    }

    // ---- file_state ----------------------------------------------------- //

    pub fn file_state_get(&self, path: &str) -> Result<Option<FileState>> {
        Ok(self
            .conn
            .query_row(
                "SELECT path, offset, size, mtime_ns, session_id FROM file_state WHERE path = ?1",
                params![path],
                |r| {
                    Ok(FileState {
                        path: r.get(0)?,
                        offset: r.get::<_, i64>(1)?.max(0) as u64,
                        size: r.get::<_, i64>(2)?.max(0) as u64,
                        mtime_ns: r.get(3)?,
                        session_id: r.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn file_state_put(&self, s: &FileState) -> Result<()> {
        self.conn.execute(
            "INSERT INTO file_state(path, offset, size, mtime_ns, session_id) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET offset = excluded.offset, size = excluded.size,
               mtime_ns = excluded.mtime_ns, session_id = excluded.session_id",
            params![s.path, s.offset as i64, s.size as i64, s.mtime_ns, s.session_id],
        )?;
        Ok(())
    }

    pub fn file_state_clear(&self) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM file_state", [])?)
    }

    // ---- kv ------------------------------------------------------------- //

    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM kv WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO kv(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

fn row_to_note(r: &rusqlite::Row<'_>) -> rusqlite::Result<Note> {
    Ok(Note {
        id: r.get(0)?,
        date: parse_date(&r.get::<_, String>(1)?).map_err(map_bad)?,
        ts: parse_ts(&r.get::<_, String>(2)?).map_err(map_bad)?,
        text: r.get(3)?,
        tags: from_json(&r.get::<_, String>(4)?),
        mentions: from_json(&r.get::<_, String>(5)?),
        source: r.get(6)?,
        created: parse_ts(&r.get::<_, String>(7)?).map_err(map_bad)?,
        updated: parse_ts(&r.get::<_, String>(8)?).map_err(map_bad)?,
        deleted: r.get::<_, i64>(9)? != 0,
    })
}

fn row_to_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: r.get(0)?,
        date: parse_date(&r.get::<_, String>(1)?).map_err(map_bad)?,
        kind: r.get(2)?,
        started: parse_ts(&r.get::<_, String>(3)?).map_err(map_bad)?,
        finished: parse_ts_opt(r.get(4)?).map_err(map_bad)?,
        status: r.get(5)?,
        error: r.get(6)?,
        duration_ms: r.get(7)?,
    })
}

fn row_to_document(r: &rusqlite::Row<'_>) -> rusqlite::Result<Document> {
    Ok(Document {
        date: parse_date(&r.get::<_, String>(0)?).map_err(map_bad)?,
        summary_md: r.get(1)?,
        full_md: r.get(2)?,
        generated_at: parse_ts(&r.get::<_, String>(3)?).map_err(map_bad)?,
        edited_at: parse_ts_opt(r.get(4)?).map_err(map_bad)?,
        run_id: r.get(5)?,
    })
}

fn row_to_repo(r: &rusqlite::Row<'_>) -> rusqlite::Result<RepoEntry> {
    Ok(RepoEntry {
        common_dir: r.get(0)?,
        path: r.get(1)?,
        name: r.get(2)?,
        source: r.get(3)?,
        first_seen: parse_ts(&r.get::<_, String>(4)?).map_err(map_bad)?,
        last_seen: parse_ts(&r.get::<_, String>(5)?).map_err(map_bad)?,
        last_commit_ts: parse_ts_opt(r.get(6)?).map_err(map_bad)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn schema_version_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("w.db");
        {
            let s = Store::open(&p).unwrap();
            assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
            s.kv_set("k", "v").unwrap();
        }
        let s = Store::open(&p).unwrap(); // 재오픈 시 마이그레이션이 데이터를 건드리지 않음
        assert_eq!(s.kv_get("k").unwrap().as_deref(), Some("v"));
        assert_eq!(s.kv_get("none").unwrap(), None);
        s.kv_set("k", "v2").unwrap();
        assert_eq!(s.kv_get("k").unwrap().as_deref(), Some("v2"));
    }

    #[test]
    fn notes_lifecycle() {
        let s = Store::open_in_memory().unwrap();
        let day = d(2026, 9, 4);
        let tags = vec!["요청".to_string()];
        let mentions = vec!["김팀장".to_string()];
        let n1 = s
            .note_add(&NewNote {
                date: day,
                ts: t("2026-09-04T01:35:00Z"),
                text: "타임아웃 늘려달라",
                tags: &tags,
                mentions: &mentions,
                source: "app",
            })
            .unwrap();
        let n2 = s
            .note_add(&NewNote {
                date: day,
                ts: t("2026-09-04T01:00:00Z"), // 더 이른 시각 → 목록에서 먼저
                text: "두 번째",
                tags: &[],
                mentions: &[],
                source: "tray",
            })
            .unwrap();
        let n3 = s
            .note_add(&NewNote {
                date: d(2026, 9, 3),
                ts: t("2026-09-03T01:00:00Z"),
                text: "다른 날",
                tags: &[],
                mentions: &[],
                source: "cli",
            })
            .unwrap();

        let list = s.notes_for(day).unwrap();
        assert_eq!(
            list.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![n2.id, n1.id]
        );
        assert_eq!(list[1].tags, tags);
        assert_eq!(list[1].mentions, mentions);
        assert_eq!(list[1].source, "app");
        assert!(!list[1].deleted);
        assert_eq!(s.notes_for(d(2026, 9, 5)).unwrap().len(), 0);

        assert!(s.note_update(n1.id, "수정됨", &[], &[]).unwrap());
        assert_eq!(s.note_get(n1.id).unwrap().unwrap().text, "수정됨");
        assert!(s.note_get(n1.id).unwrap().unwrap().tags.is_empty());

        assert!(s.note_delete(n1.id).unwrap());
        assert!(!s.note_delete(n1.id).unwrap()); // 두 번째 삭제는 false
        assert!(!s.note_update(n1.id, "x", &[], &[]).unwrap()); // 삭제된 메모는 갱신 불가
        assert_eq!(s.notes_for(day).unwrap().len(), 1);
        assert!(s.note_get(n1.id).unwrap().unwrap().deleted); // 행은 남아 있음(소프트)
        assert_eq!(s.note_dates(10).unwrap(), vec![day, d(2026, 9, 3)]);
        assert_eq!(s.note_dates(1).unwrap(), vec![day]); // limit
        // 그날 메모가 전부 삭제되면 날짜도 빠진다
        assert!(s.note_delete(n3.id).unwrap());
        assert_eq!(s.note_dates(10).unwrap(), vec![day]);
        assert!(s.note_get(9999).unwrap().is_none());
    }

    #[test]
    fn runs_lifecycle_and_abandon() {
        let s = Store::open_in_memory().unwrap();
        let id = s.run_start(d(2026, 9, 3), run_kind::MANUAL).unwrap();
        assert_eq!(s.runs_running().unwrap().len(), 1);
        let r = s.run_get(id).unwrap().unwrap();
        assert_eq!(r.status, run_status::RUNNING);
        assert!(r.finished.is_none());

        s.run_finish(id, run_status::FAILED, Some("claude CLI 응답 없음"))
            .unwrap();
        let r = s.run_get(id).unwrap().unwrap();
        assert_eq!(r.status, run_status::FAILED);
        assert_eq!(r.error.as_deref(), Some("claude CLI 응답 없음"));
        assert!(r.finished.is_some());
        assert!(r.duration_ms.unwrap() >= 0);
        assert!(s.runs_running().unwrap().is_empty());

        let id2 = s.run_start(d(2026, 9, 4), run_kind::AUTO).unwrap();
        s.run_finish(id2, run_status::OK, None).unwrap();
        let recent = s.runs_recent(10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].id, id2); // 최신 먼저
        assert_eq!(s.runs_recent(1).unwrap().len(), 1);

        // 비정상 종료 정리: 소요 시간 없음, running 이 아닌 실행은 건드리지 않음
        let id3 = s.run_start(d(2026, 9, 5), run_kind::MANUAL).unwrap();
        s.run_abandon(id3, Some("앱 종료")).unwrap();
        let r = s.run_get(id3).unwrap().unwrap();
        assert_eq!(r.status, run_status::ABANDONED);
        assert_eq!(r.duration_ms, None);
        assert!(r.finished.is_some());
        s.run_abandon(id2, None).unwrap();
        assert_eq!(s.run_get(id2).unwrap().unwrap().status, run_status::OK);
    }

    #[test]
    fn documents_upsert_edit_and_calendar() {
        let s = Store::open_in_memory().unwrap();
        let day = d(2026, 9, 3);
        let doc = Document {
            date: day,
            summary_md: Some("## 요약".into()),
            full_md: "# 일지\n## 요약".into(),
            generated_at: t("2026-09-03T09:42:00Z"),
            edited_at: None,
            run_id: Some(1),
        };
        s.document_put(&doc).unwrap();
        assert_eq!(s.document_get(day).unwrap().unwrap(), doc);
        assert!(s.document_get(d(2026, 9, 4)).unwrap().is_none());

        assert!(s.document_mark_edited(day, "# 일지 (편집)", None).unwrap());
        let e = s.document_get(day).unwrap().unwrap();
        assert!(e.is_edited());
        assert_eq!(e.full_md, "# 일지 (편집)");
        assert_eq!(e.summary_md.as_deref(), Some("## 요약")); // None 이면 기존 유지
        assert!(
            s.document_mark_edited(day, "# 본문", Some("## 새 요약"))
                .unwrap()
        );
        assert_eq!(
            s.document_get(day).unwrap().unwrap().summary_md.as_deref(),
            Some("## 새 요약")
        );
        assert!(!s.document_mark_edited(d(2026, 9, 4), "x", None).unwrap());

        // 다시 생성하면(편집본을 그대로 넘겨도) 편집 표식이 지워진다.
        let mut regen = s.document_get(day).unwrap().unwrap();
        regen.generated_at = t("2026-09-03T10:00:00Z");
        regen.run_id = Some(2);
        assert!(regen.edited_at.is_some());
        s.document_put(&regen).unwrap();
        let after = s.document_get(day).unwrap().unwrap();
        assert!(!after.is_edited());
        assert_eq!(after.run_id, Some(2));

        s.document_put(&Document {
            date: d(2026, 9, 1),
            ..regen.clone()
        })
        .unwrap();
        s.document_mark_edited(d(2026, 9, 1), "e", None).unwrap();
        // 양끝 포함
        assert_eq!(
            s.document_statuses(d(2026, 9, 1), d(2026, 9, 3)).unwrap(),
            vec![
                DocStatus {
                    date: d(2026, 9, 1),
                    edited: true
                },
                DocStatus {
                    date: day,
                    edited: false
                }
            ]
        );
        assert!(
            s.document_statuses(d(2026, 9, 2), d(2026, 9, 2))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            s.document_statuses(d(2026, 9, 3), d(2026, 9, 30))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(s.document_dates(1).unwrap(), vec![day]);
        assert!(s.document_delete(day).unwrap());
        assert!(!s.document_delete(day).unwrap());
    }

    #[test]
    fn repos_cache_first_seen_and_newest_commit_ts() {
        let s = Store::open_in_memory().unwrap();
        let seen = t("2026-09-01T00:00:00Z");
        let mut r = RepoEntry {
            common_dir: "D:/a/.git".into(),
            path: "D:/a".into(),
            name: "a".into(),
            source: "scan".into(),
            first_seen: seen,
            last_seen: seen,
            last_commit_ts: None, // 처음엔 커밋 정보 없음
        };
        s.repo_upsert(&r).unwrap();
        // None → Some 은 반영, 더 오래된 값·None 은 기존 유지, first_seen 은 절대 안 바뀜
        r.last_commit_ts = Some(t("2026-09-02T00:00:00Z"));
        r.first_seen = t("2026-09-10T00:00:00Z");
        r.last_seen = t("2026-09-10T00:00:00Z");
        r.source = "claude".into();
        s.repo_upsert(&r).unwrap();
        r.last_commit_ts = Some(t("2026-08-01T00:00:00Z"));
        s.repo_upsert(&r).unwrap();
        r.last_commit_ts = None;
        s.repo_upsert(&r).unwrap();
        let all = s.repos_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].last_commit_ts, Some(t("2026-09-02T00:00:00Z")));
        assert_eq!(all[0].first_seen, seen);
        assert_eq!(all[0].last_seen, t("2026-09-10T00:00:00Z"));
        assert_eq!(all[0].source, "claude");
        r.last_commit_ts = Some(t("2026-09-05T00:00:00Z"));
        s.repo_upsert(&r).unwrap();
        assert_eq!(
            s.repos_all().unwrap()[0].last_commit_ts,
            Some(t("2026-09-05T00:00:00Z"))
        );

        // 새로 발견됐지만 커밋이 없는 저장소는 first_seen 으로 걸린다
        s.repo_upsert(&RepoEntry {
            common_dir: "D:/b/.git".into(),
            path: "D:/b".into(),
            name: "b".into(),
            source: "scan".into(),
            first_seen: t("2026-09-10T00:00:00Z"),
            last_seen: t("2026-09-12T00:00:00Z"),
            last_commit_ts: None,
        })
        .unwrap();
        let names = |since: &str| {
            s.repos_active_since(&t(since))
                .unwrap()
                .into_iter()
                .map(|r| r.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names("2026-09-04T00:00:00Z"), vec!["a", "b"]);
        assert_eq!(names("2026-09-05T00:00:00Z"), vec!["a", "b"]); // 경계 포함(==last_commit_ts)
        assert_eq!(names("2026-09-06T00:00:00Z"), vec!["b"]); // a 는 빠지고 b 는 first_seen 으로
        assert_eq!(names("2026-09-10T00:00:00Z"), vec!["b"]); // 경계 포함(==first_seen)
        assert_eq!(names("2026-09-11T00:00:00Z"), Vec::<String>::new()); // last_seen 은 보지 않음
        assert!(s.repo_delete("D:/a/.git").unwrap());
        assert_eq!(s.repos_all().unwrap().len(), 1);
    }

    #[test]
    fn file_state_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.file_state_get("p").unwrap().is_none());
        let fs1 = FileState {
            path: "p".into(),
            offset: 10,
            size: 20,
            mtime_ns: 123,
            session_id: Some("sid".into()),
        };
        s.file_state_put(&fs1).unwrap();
        assert_eq!(s.file_state_get("p").unwrap().unwrap(), fs1);
        let fs2 = FileState {
            offset: 20,
            size: 30,
            ..fs1.clone()
        };
        s.file_state_put(&fs2).unwrap();
        assert_eq!(s.file_state_get("p").unwrap().unwrap(), fs2);
        assert_eq!(s.file_state_clear().unwrap(), 1);
        assert!(s.file_state_get("p").unwrap().is_none());
    }
}
