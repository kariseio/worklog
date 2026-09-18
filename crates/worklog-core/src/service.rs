//! 오케스트레이션 서비스 — CLI 와 데스크톱 앱이 공유하는 핵심 로직.
//!
//! 수집기 실행 순서/결합, 요약, 문서 조합, 저장, 히스토리 조회를 한곳에 모아
//! `worklog-cli` 와 Tauri 앱이 동일하게 호출한다.

use std::{borrow::Cow, collections::HashMap, path::Path};

use chrono::{Datelike, NaiveDate};
use chrono_tz::Tz;
use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::{
    analyze::{Analysis, analyze},
    collect::{
        CollectContext, Collector, CollectorResult, SourceStatus, claude::ClaudeCollector,
        codex::CodexCollector, git::GitCollector, naverworks::NaverWorksCollector,
    },
    config::{Config, SOURCE_NAMES},
    exclude::{Excluder, PRIVATE_PROJECT, filter_for_signal, redact_for_document},
    model::{DailyData, NoteItem, WorkLog},
    notes,
    output::{
        Sink, SinkResult, markdown::MarkdownSink, notion::NotionSink, obsidian::ObsidianSink,
    },
    paths,
    render::{
        is_meta_session, render_analysis, render_facts, render_session_blocks,
        render_session_section, render_timeline_for_llm, render_work_signal,
    },
    store::Store,
    summarize::Summarizer,
    template,
    time::{TimeError, fmt_time, get_tz, parse_iso_in, resolve_day},
    weekly::{self, RangeKpis},
};

pub const ALL_SOURCES: [&str; 4] = SOURCE_NAMES;

// --------------------------------------------------------------------------- //
// 컨텍스트
// --------------------------------------------------------------------------- //

pub fn make_context(cfg: &Config, date_spec: Option<&str>) -> Result<CollectContext, TimeError> {
    let tz = get_tz(&cfg.timezone);
    let day = resolve_day(date_spec, tz)?;
    Ok(CollectContext::new(day, cfg.timezone.clone()))
}

/// 실제로 돌릴 소스. `requested` 가 있으면 (설정의 켜짐 여부와 무관하게) 그 목록만.
pub fn enabled_sources(cfg: &Config, requested: Option<&[String]>) -> Vec<&'static str> {
    match requested {
        Some(req) if !req.is_empty() => {
            let wanted: Vec<&str> = req
                .iter()
                .flat_map(|s| s.split(','))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            ALL_SOURCES
                .iter()
                .copied()
                .filter(|n| wanted.contains(n))
                .collect()
        }
        _ => cfg.enabled_sources(),
    }
}

// --------------------------------------------------------------------------- //
// 수집
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone, PartialEq)]
pub struct Collected {
    pub data: DailyData,
    /// git · claude · codex · naverworks 순서.
    pub statuses: Vec<SourceStatus>,
}

fn absorb<T>(name: &str, res: &CollectorResult<T>, data: &mut DailyData) {
    for w in &res.warnings {
        tracing::warn!("[{name}] {w}");
        data.warnings.push(format!("[{name}] {w}"));
    }
    if res.skipped {
        let reason = res.skip_reason.clone().unwrap_or_default();
        tracing::info!("[{name}] 건너뜀: {reason}");
        data.warnings.push(format!("[{name}] 건너뜀: {reason}"));
    }
}

fn session_count(d: &crate::model::SessionData) -> usize {
    d.sessions.iter().filter(|s| !is_meta_session(s)).count()
}

/// 소스들을 수집한다. Claude 로그를 먼저(git 자동탐색에 cwd 를 넘기므로), 나머지는 병렬로.
/// `notes` 는 그날 메모(저장소에서 읽어 넘김).
pub fn collect(
    cfg: &Config,
    ctx: &CollectContext,
    sources: &[&str],
    notes: Vec<NoteItem>,
) -> Collected {
    let mut data = DailyData::new(ctx.target_date(), cfg.timezone.clone());
    data.notes = notes;
    let mut statuses: IndexMap<&str, SourceStatus> = ALL_SOURCES
        .iter()
        .map(|n| (*n, SourceStatus::disabled(n)))
        .collect();

    let mut claude_cwds: Vec<String> = Vec::new();
    if sources.contains(&"claude") {
        let res = ClaudeCollector::new(cfg.sources.claude.clone()).collect(ctx);
        absorb("claude", &res, &mut data);
        // 칩 수·지표·본문과 일치하도록 자동요약(meta) 세션은 세지 않는다.
        statuses.insert("claude", SourceStatus::from_result(&res, session_count));
        if let Some(d) = res.data {
            claude_cwds = d.cwds().iter().map(|s| s.to_string()).collect();
            data.claude = Some(d);
        }
    }

    let want_git = sources.contains(&"git");
    let want_codex = sources.contains(&"codex");
    let want_nw = sources.contains(&"naverworks");
    let (git_res, codex_res, nw_res) = std::thread::scope(|s| {
        let git_h = want_git.then(|| {
            let cwds = claude_cwds.clone();
            s.spawn(move || {
                // 저장소 목록을 먼저 한 번만 확정한다(커밋 0 의 이유에 쓸 '저장소 N개').
                // 그 목록을 그대로 넘기므로 디스크 탐색은 여전히 한 번이다.
                let collector = GitCollector::new(cfg.sources.git.clone()).with_extra_repos(cwds);
                let repos: Vec<std::path::PathBuf> = collector
                    .resolve_repos(&mut Vec::new())
                    .into_iter()
                    .map(|r| r.path)
                    .collect();
                let scanned = repos.len();
                (scanned, collector.with_known_repos(repos).collect(ctx))
            })
        });
        let codex_h = want_codex
            .then(|| s.spawn(|| CodexCollector::new(cfg.sources.codex.clone()).collect(ctx)));
        let nw_h = want_nw.then(|| {
            s.spawn(|| NaverWorksCollector::new(cfg.sources.naverworks.clone()).collect(ctx))
        });
        (
            git_h.map(|h| {
                h.join()
                    .unwrap_or_else(|_| (0, CollectorResult::fail("git", "예외: 수집 스레드 패닉")))
            }),
            codex_h.map(|h| {
                h.join()
                    .unwrap_or_else(|_| CollectorResult::fail("codex", "예외: 수집 스레드 패닉"))
            }),
            nw_h.map(|h| {
                h.join().unwrap_or_else(|_| {
                    CollectorResult::fail("naverworks", "예외: 수집 스레드 패닉")
                })
            }),
        )
    });

    if let Some((scanned, res)) = git_res {
        absorb("git", &res, &mut data);
        statuses.insert("git", SourceStatus::from_result(&res, |d| d.commits.len()));
        data.git = res.data;
        data.git_repos_scanned = scanned;
        data.git_authors = git_author_count(&cfg.sources.git);
    }
    if let Some(res) = codex_res {
        absorb("codex", &res, &mut data);
        statuses.insert("codex", SourceStatus::from_result(&res, session_count));
        data.codex = res.data;
    }
    if let Some(res) = nw_res {
        absorb("naverworks", &res, &mut data);
        statuses.insert(
            "naverworks",
            SourceStatus::from_result(&res, |d| d.events.len()),
        );
        data.calendar = res.data;
    }

    normalize_sessions(&mut data);
    disambiguate_repo_names(&mut data);
    // 칩 수도 병합 뒤 세션 수로 맞춘다(지표의 'AI N세션'과 어긋나지 않게).
    for (name, sd) in [("claude", &data.claude), ("codex", &data.codex)] {
        if let (Some(st), Some(sd)) = (statuses.get_mut(name), sd)
            && st.state == crate::collect::SourceState::Ok
        {
            st.count = session_count(sd);
        }
    }
    Collected {
        data,
        statuses: statuses.into_values().collect(),
    }
}

/// 커밋 매칭에 쓰는 '내 작성자' 신원 수 — `커밋 0 — 저장소 N개에서 내 작성자(M개)…` 의 M.
///
/// 명시 `author` 가 있으면 그것 하나, 없으면 추가 신원 + 자동 감지 신원(중복 제거).
/// 저장소마다 다른 `user.email` 까지는 세지 않는 근사값이다.
pub(crate) fn git_author_count(cfg: &crate::config::GitConfig) -> usize {
    use crate::collect::git::{detected_identities, identity_pattern};
    if !cfg.author.trim().is_empty() {
        return 1;
    }
    let mut pats: Vec<String> = cfg
        .authors
        .iter()
        .filter_map(|a| identity_pattern(a))
        .collect();
    pats.extend(
        detected_identities()
            .iter()
            .filter_map(|a| identity_pattern(a)),
    );
    pats.sort();
    pats.dedup();
    pats.len()
}

// --------------------------------------------------------------------------- //
// 세션 정규화 (중복 병합 + worktree 롤업)
// --------------------------------------------------------------------------- //

/// 같은 세션이 여러 파일로 들어온 것을 하나로 합치고, worktree cwd 를 실제 저장소로 묶는다.
///
/// 하나의 Claude/Codex 세션이 worktree 별 로그 파일이나 이어받기(resume)로 여러 파일에 걸쳐
/// 기록되면 수집기는 파일마다 세션 하나를 만든다. 그대로 두면 세션 수·집중시간·토큰이 배로
/// 부풀고, worktree 마다 프로젝트 행이 따로 생긴다. 여기서:
///
/// 1. [`Session::dedupe_key`] 가 같은 세션들을 하나로 병합한다.
///    구간은 `[min(first_ts), max(last_ts)]`, 파일·명령은 합집합, 도구 호출 수는 합,
///    출력 토큰은 **최댓값**(같은 세션을 두 번 읽은 것이므로 더하면 이중 계산),
///    제목·의도는 비어 있지 않은 쪽, 질답은 `(시각, 질문)` 기준 중복 제거.
/// 2. cwd 가 git 저장소면 `git-common-dir` 로 실제 저장소를 찾아 `project` 를 그 이름으로
///    바꾼다. worktree 들은 common-dir 을 공유하므로 한 프로젝트로 모인다.
///
/// [`collect`] 와 [`crate::live::Live`] 재조립 양쪽에서 [`disambiguate_repo_names`] 직전에
/// 부른다. 여러 번 불러도 결과가 같다(멱등).
pub fn normalize_sessions(data: &mut DailyData) {
    let mut cache: HashMap<String, Option<RepoInfo>> = HashMap::new();
    if let Some(sd) = &mut data.claude {
        normalize_session_list(&mut sd.sessions, &mut cache);
    }
    if let Some(sd) = &mut data.codex {
        normalize_session_list(&mut sd.sessions, &mut cache);
    }
}

fn normalize_session_list(
    sessions: &mut Vec<crate::model::Session>,
    cache: &mut HashMap<String, Option<RepoInfo>>,
) {
    use crate::model::Session;
    use indexmap::map::Entry;

    let mut merged: IndexMap<String, Session> = IndexMap::with_capacity(sessions.len());
    for s in sessions.drain(..) {
        match merged.entry(s.dedupe_key()) {
            Entry::Occupied(mut e) => merge_session(e.get_mut(), s),
            Entry::Vacant(e) => {
                e.insert(s);
            }
        }
    }
    let mut out: Vec<Session> = merged.into_values().collect();
    for s in &mut out {
        if let Some(cwd) = s.cwd.clone()
            && let Some((name, root)) = repo_of(&cwd, cache)
        {
            s.project = Some(name);
            s.repo_root = Some(root);
        }
    }
    out.sort_by_key(|s| (s.first_ts.is_none(), s.first_ts));
    *sessions = out;
}

/// (저장소 이름, 물리 저장소 루트 경로) — worktree 면 둘 다 **본체** 것이다.
type RepoInfo = (String, String);

/// cwd → 실제 저장소 이름·루트(worktree 는 본체). 저장소가 아니면 None. 같은 cwd 는 한 번만 본다.
///
/// 루트까지 세션에 남기는 이유: 제외 글롭이 형제 worktree 경로를 알 리 없으므로
/// (`C:\…\workspaces\foo\wt` 는 `D:\works\foo\**` 에 걸리지 않는다) 물리 저장소를
/// 들고 있어야 [`crate::exclude`] 가 같은 저장소로 알아본다.
fn repo_of(cwd: &str, cache: &mut HashMap<String, Option<RepoInfo>>) -> Option<RepoInfo> {
    if let Some(v) = cache.get(cwd) {
        return v.clone();
    }
    let v = crate::collect::git::identify(Path::new(cwd)).map(|i| {
        let root = crate::collect::git::repo_root_of(Path::new(&i.common_dir));
        (i.name, root.to_string_lossy().into_owned())
    });
    cache.insert(cwd.to_string(), v.clone());
    v
}

/// 세션 구간 길이(초). 시각을 모르면 -1 — 아는 쪽이 항상 이긴다.
fn span_secs(s: &crate::model::Session) -> i64 {
    match (s.first_ts, s.last_ts) {
        (Some(f), Some(l)) => (l - f).num_seconds().max(0),
        _ => -1,
    }
}

fn is_blank(v: &Option<String>) -> bool {
    v.as_deref().map(str::trim).unwrap_or("").is_empty()
}

/// 등장 순서를 지키며 `extra` 중 없는 값만 뒤에 붙인다.
fn union_strings(base: &mut Vec<String>, extra: Vec<String>) {
    for v in extra {
        if !base.contains(&v) {
            base.push(v);
        }
    }
}

fn merge_session(base: &mut crate::model::Session, other: crate::model::Session) {
    // 더 넓은 구간을 가진 쪽의 식별 정보를 남긴다(짧은 조각이 cwd 를 덮어쓰지 않도록).
    let wider = span_secs(&other) > span_secs(base);
    base.first_ts = min_opt(base.first_ts, other.first_ts);
    base.last_ts = max_opt(base.last_ts, other.last_ts);
    if base.session_id.is_none() {
        base.session_id = other.session_id;
    }
    if other.cwd.is_some() && (wider || base.cwd.is_none()) {
        base.cwd = other.cwd;
    }
    if other.project.is_some() && (wider || base.project.is_none()) {
        base.project = other.project;
    }
    if other.repo_root.is_some() && (wider || base.repo_root.is_none()) {
        base.repo_root = other.repo_root;
    }
    if other.git_branch.is_some() && (wider || base.git_branch.is_none()) {
        base.git_branch = other.git_branch;
    }
    if is_blank(&base.title) && !is_blank(&other.title) {
        base.title = other.title;
    }
    if is_blank(&base.intent) && !is_blank(&other.intent) {
        base.intent = other.intent;
    }
    union_strings(&mut base.files_edited, other.files_edited);
    union_strings(&mut base.files_read, other.files_read);
    union_strings(&mut base.commands, other.commands);
    for (tool, n) in other.tool_counts {
        *base.tool_counts.entry(tool).or_insert(0) += n;
    }
    // 같은 세션을 두 번 읽은 것이므로 더하지 않고 큰 쪽을 쓴다.
    base.output_tokens = base.output_tokens.max(other.output_tokens);
    base.qa_dropped = base.qa_dropped.max(other.qa_dropped);
    merge_qa(&mut base.qa, other.qa);
}

/// 질답 병합 — `(시각, 질문)` 이 같으면 같은 턴으로 보고 버린다. 새로 붙었으면 시각순으로 정렬.
fn merge_qa(base: &mut Vec<crate::model::QaTurn>, extra: Vec<crate::model::QaTurn>) {
    let mut added = false;
    for t in extra {
        if base
            .iter()
            .any(|b| b.time == t.time && b.question == t.question)
        {
            continue;
        }
        base.push(t);
        added = true;
    }
    if added {
        // 시각을 모르는 턴("")은 뒤로. 같은 시각끼리는 원래 순서 유지(안정 정렬).
        base.sort_by(|a, b| {
            (a.time.is_empty(), a.time.as_str()).cmp(&(b.time.is_empty(), b.time.as_str()))
        });
    }
}

fn min_opt<T: Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

fn max_opt<T: Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// 동명(basename 동일)이지만 물리적으로 다른 저장소를 상위 폴더로 구분한다.
///
/// 예: `D:\kms\private\kms_frontend` 와 `D:\works\kms_frontend` 는 둘 다 'kms_frontend'
/// → 'private/kms_frontend', 'works/kms_frontend'. git 커밋과 세션이 같은 키(git-common-dir)를
/// 공유하므로 서로 어긋나지 않는다.
pub fn disambiguate_repo_names(data: &mut DailyData) {
    use crate::collect::git::{common_dir_of, repo_root_of};

    // 저장소키 → basename (첫 등장 순서)
    let mut key_base: IndexMap<String, String> = IndexMap::new();
    if let Some(git) = &data.git {
        for c in &git.commits {
            if !c.repo_path.is_empty() {
                key_base
                    .entry(c.repo_path.clone())
                    .or_insert_with(|| c.repo.clone());
            }
        }
    }
    // 세션 → 저장소키 (claude 인덱스, codex 인덱스 순)
    let mut sess_keys: Vec<Option<String>> = Vec::new();
    for s in data.all_sessions() {
        let k = s
            .cwd
            .as_deref()
            .and_then(|c| common_dir_of(Path::new(c)))
            .map(|p| p.to_string_lossy().into_owned());
        if let Some(k) = &k {
            key_base
                .entry(k.clone())
                .or_insert_with(|| s.project.clone().unwrap_or_else(|| "?".into()));
        }
        sess_keys.push(k);
    }

    let mut keys_by_base: IndexMap<String, Vec<String>> = IndexMap::new();
    for (key, base) in &key_base {
        keys_by_base
            .entry(base.clone())
            .or_default()
            .push(key.clone());
    }

    let mut newname: HashMap<String, String> = HashMap::new();
    for (base, keys) in keys_by_base {
        if keys.len() <= 1 {
            for k in keys {
                newname.insert(k, base.clone());
            }
            continue;
        }
        // 충돌 → 상위 폴더로 구분
        let mut cand: IndexMap<String, String> = IndexMap::new();
        for k in &keys {
            let root = repo_root_of(Path::new(k));
            let parent = root
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let nm = if parent.is_empty() {
                base.clone()
            } else {
                format!("{parent}/{base}")
            };
            cand.insert(k.clone(), nm);
        }
        // 상위 폴더까지 같아 여전히 겹치면 짧은 해시로 유일화
        let mut dup: IndexMap<String, Vec<String>> = IndexMap::new();
        for (k, nm) in &cand {
            dup.entry(nm.clone()).or_default().push(k.clone());
        }
        for (nm, ks) in dup {
            if ks.len() == 1 {
                newname.insert(ks[0].clone(), nm);
            } else {
                for k in ks {
                    let h = short_hash(&k);
                    newname.insert(k, format!("{nm}#{h}"));
                }
            }
        }
    }

    if let Some(git) = &mut data.git {
        for c in &mut git.commits {
            if let Some(n) = newname.get(&c.repo_path) {
                c.repo = n.clone();
            }
        }
    }
    let mut i = 0usize;
    let mut apply = |sessions: &mut Vec<crate::model::Session>| {
        for s in sessions.iter_mut() {
            if let Some(Some(k)) = sess_keys.get(i)
                && let Some(n) = newname.get(k)
            {
                s.project = Some(n.clone());
            }
            i += 1;
        }
    };
    if let Some(c) = &mut data.claude {
        apply(&mut c.sessions);
    }
    if let Some(c) = &mut data.codex {
        apply(&mut c.sessions);
    }
}

/// 경로 문자열 → 6자리 16진 지문(FNV-1a 64). 동명·동상위 저장소 유일화용.
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:06x}", h & 0xff_ffff)
}

// --------------------------------------------------------------------------- //
// 문서 조합 / 요약 / 생성
// --------------------------------------------------------------------------- //

/// 수집 소스 + 저장 대상의 유무를 한눈에 보여주는 라벨. (앱 화면 칩·요약 프롬프트 머리글용)
pub fn availability_line(cfg: &Config, statuses: &[SourceStatus]) -> String {
    use crate::collect::SourceState;

    let label = |n: &str| -> String {
        match n {
            "git" => "Git".into(),
            "claude" => "Claude".into(),
            "codex" => "Codex".into(),
            "naverworks" => "캘린더(NaverWorks)".into(),
            other => other.to_string(),
        }
    };
    let icon = |st: SourceState| match st {
        SourceState::Ok => "✅",
        SourceState::Error => "⚠️",
        SourceState::Skipped | SourceState::Disabled => "❌",
    };
    let order = |n: &str| ALL_SOURCES.iter().position(|x| *x == n).unwrap_or(9);
    let mut sorted: Vec<&SourceStatus> = statuses.iter().collect();
    sorted.sort_by_key(|s| order(&s.name));
    let src: Vec<String> = sorted
        .iter()
        .map(|s| {
            let cnt = if s.state == SourceState::Ok && s.count > 0 {
                format!("({})", s.count)
            } else {
                String::new()
            };
            format!("{} {}{cnt}", label(&s.name), icon(s.state))
        })
        .collect();

    // 실제 저장은 enabled 로 결정되므로(save 와 일치), '켜짐 그리고 설정됨'을 기준으로 표기.
    let ob = &cfg.outputs.obsidian;
    let no = &cfg.outputs.notion;
    let yn = |b: bool| if b { "✅" } else { "❌" };
    let out = [
        format!("로컬 md {}", yn(cfg.outputs.markdown.enabled)),
        format!(
            "Obsidian {}",
            yn(ob.enabled && !ob.vault_dir.trim().is_empty())
        ),
        format!("Notion {}", yn(no.is_configured())),
    ];
    format!(
        "수집 소스: {}\n저장 대상: {}",
        src.join(" · "),
        out.join(" · ")
    )
}

/// 수집 데이터로부터 렌더 산출물 일체(요약 입력 신호 포함).
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub facts: String,
    pub availability: String,
    pub analysis: Analysis,
    pub analysis_md: String,
    /// 요약기에 넣을 신호(정제 신호 + 시간순 이벤트 + 세션 질답).
    pub signal: String,
}

/// 수집 데이터 → 렌더 산출물([`Rendered`]).
///
/// 제외 목록(`sources.exclude`, product-plan §4 원칙 4 · §5-1 N0)이 걸리면 데이터를 **두 갈래**로
/// 나눈다. 문서(사실 정리·지표)는 걸린 항목을 [`PRIVATE_PROJECT`] 한 행의 집계로만 남기고,
/// 요약기에 보내는 `signal` 에서는 그 세션·커밋을 통째로 뺀다 — 이름도 경로도 질답도 나가지 않는다.
pub fn render_all(cfg: &Config, data: &DailyData, statuses: &[SourceStatus], tz: Tz) -> Rendered {
    let ex = Excluder::new(&cfg.sources.exclude);
    let hit = ex.hits(data);
    if hit {
        tracing::info!(
            "제외 목록 {}건 적용 — 걸린 세션·커밋은 요약에 보내지 않고 문서에는 {} 집계만 남깁니다",
            ex.patterns().len(),
            PRIVATE_PROJECT
        );
    }
    let doc: Cow<DailyData> = if hit {
        Cow::Owned(redact_for_document(data, &ex))
    } else {
        Cow::Borrowed(data)
    };
    let sig: Cow<DailyData> = if hit {
        Cow::Owned(filter_for_signal(data, &ex))
    } else {
        Cow::Borrowed(data)
    };

    let facts = render_facts(&doc, tz);
    let availability = availability_line(cfg, statuses);
    let analysis = analyze(&doc, tz);
    let analysis_md = render_analysis(&analysis);
    let mut signal = render_work_signal(
        &sig,
        tz,
        &format!("가용 데이터 — {}", availability.replace('\n', " / ")),
    );
    // 타임라인도 '보낼 데이터'로 다시 뽑는다 — 문서용 분석에는 제외 항목의 구간이 들어 있다.
    let sig_analysis: Cow<Analysis> = if hit {
        Cow::Owned(analyze(&sig, tz))
    } else {
        Cow::Borrowed(&analysis)
    };
    let tl = render_timeline_for_llm(&sig_analysis);
    if !tl.is_empty() {
        signal = format!("{signal}\n{tl}");
    }
    let section = render_session_section(&render_session_blocks(&sig, tz, 8));
    if !section.is_empty() {
        signal = format!("{}\n\n{section}", signal.trim_end());
    }
    Rendered {
        facts,
        availability,
        analysis,
        analysis_md,
        signal,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GenerateResult {
    pub worklog: WorkLog,
    pub statuses: Vec<SourceStatus>,
    pub rendered: Rendered,
}

/// 기본 경로의 메모 저장소를 연다. 실패하면 경고 후 None(메모 없이 진행).
pub fn open_store() -> Option<Store> {
    match Store::open(&paths::db_path()) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(
                "메모 저장소를 열 수 없습니다({}): {e}",
                paths::db_path().display()
            );
            None
        }
    }
}

/// 수집 → (정제 신호) 요약 → 조합까지 한 번에. (저장은 별도) 메모는 기본 저장소에서 읽는다.
/// 템플릿은 설정값(`cfg.summarizer.template`)을 쓴다.
pub fn generate(
    cfg: &Config,
    date_spec: Option<&str>,
    no_llm: bool,
    sources: Option<&[String]>,
) -> Result<GenerateResult, TimeError> {
    let summarizer = Summarizer::new(cfg.summarizer.clone());
    let store = open_store();
    generate_with(
        cfg,
        date_spec,
        no_llm,
        sources,
        &summarizer,
        store.as_ref(),
        None,
    )
}

/// `template` 이 None 이면 `cfg.summarizer.template`, 모르는 id 면 `standard`.
pub fn generate_with(
    cfg: &Config,
    date_spec: Option<&str>,
    no_llm: bool,
    sources: Option<&[String]>,
    summarizer: &Summarizer,
    store: Option<&Store>,
    template: Option<&str>,
) -> Result<GenerateResult, TimeError> {
    let tpl = template::resolve(template.unwrap_or(&cfg.summarizer.template));
    let ctx = make_context(cfg, date_spec)?;
    let wanted = enabled_sources(cfg, sources);
    let note_items = notes::items_for(store, ctx.target_date());
    let Collected { data, statuses } = collect(cfg, &ctx, &wanted, note_items);
    let rendered = render_all(cfg, &data, &statuses, ctx.tz());
    let summary = if !no_llm && !data.is_empty() {
        summarizer.summarize_day_with(
            &template::system_prompt(tpl),
            &rendered.signal,
            &ctx.target_date().to_string(),
            &rendered.availability,
        )
    } else {
        None
    };
    let full = template::compose(
        tpl,
        ctx.target_date(),
        summary.as_deref(),
        &rendered.analysis,
        ctx.tz(),
        &rendered.facts,
        cfg.include_raw_data,
    );
    let worklog = WorkLog {
        target_date: ctx.target_date(),
        facts_markdown: rendered.facts.clone(),
        full_markdown: full,
        data,
        summary_markdown: summary,
    };
    Ok(GenerateResult {
        worklog,
        statuses,
        rendered,
    })
}

// --------------------------------------------------------------------------- //
// 주간 모아보기 (N8) — 읽기만 한다
// --------------------------------------------------------------------------- //

#[derive(Debug, thiserror::Error)]
pub enum RangeError {
    #[error("기간이 거꾸로입니다: {from} ~ {to}")]
    BadRange { from: NaiveDate, to: NaiveDate },
    #[error("문서를 읽지 못했습니다: {0}")]
    Store(#[from] crate::store::StoreError),
}

/// [`generate_range`] 결과. `markdown` 외 나머지는 로그·테스트·호출자 표시용.
#[derive(Debug, Clone, PartialEq)]
pub struct RangeDocument {
    /// 완성된 주간 문서. **저장하지 않는다** — stdout 또는 `--out` 파일로만.
    pub markdown: String,
    /// 문서가 있어 실제로 합쳐진 날.
    pub days_included: Vec<NaiveDate>,
    /// 문서가 없어 경고로 남은 **평일**(주말은 조용히 건너뛴다).
    pub days_missing: Vec<NaiveDate>,
    /// 섹션을 찾지 못해 원문을 그대로 붙인 날.
    pub parse_failures: Vec<NaiveDate>,
    pub kpis: RangeKpis,
    /// 중복 병합 LLM 호출을 실제로 썼는가.
    pub llm_used: bool,
}

fn is_weekday(d: NaiveDate) -> bool {
    d.weekday().num_days_from_monday() < 5
}

/// `[from, to]` 구간의 **이미 만들어진** 일별 문서를 파싱해 주간 문서 한 장으로 합친다.
///
/// `documents` 를 읽기만 한다 — 어떤 저장소·싱크에도 쓰지 않고 스키마도 그대로다(N8).
/// `summarizer` 가 `Some` 이면 중복 병합에만 LLM 을 **1회** 호출하고, 실패하거나 형식을 벗어나면
/// 결정론적 병합본을 그대로 쓴다.
pub fn generate_range(
    cfg: &Config,
    store: &Store,
    from: NaiveDate,
    to: NaiveDate,
    template_id: &str,
    summarizer: Option<&Summarizer>,
) -> Result<RangeDocument, RangeError> {
    if to < from {
        return Err(RangeError::BadRange { from, to });
    }
    if !template::is_weekly(template_id) {
        tracing::warn!(
            "기간 문서 템플릿은 '{}' 하나뿐입니다 — {template_id:?} 대신 이걸로 만듭니다.",
            template::WEEKLY_TEMPLATE
        );
    }
    tracing::info!("주간 모아보기 {from} ~ {to} ({})", cfg.timezone);

    let statuses = store.document_statuses(from, to)?;
    let mut days: Vec<weekly::DayDoc> = Vec::with_capacity(statuses.len());
    for st in &statuses {
        let Some(doc) = store.document_get(st.date)? else {
            continue;
        };
        days.push(weekly::parse_day(
            st.date,
            &doc.full_md,
            st.edited || doc.is_edited(),
        ));
    }

    let included: Vec<NaiveDate> = days.iter().map(|d| d.date).collect();
    let missing: Vec<NaiveDate> = from
        .iter_days()
        .take_while(|d| *d <= to)
        .filter(|d| is_weekday(*d) && !included.contains(d))
        .collect();
    let failures: Vec<(NaiveDate, String)> = days
        .iter()
        .filter(|d| d.parse_failed)
        .map(|d| (d.date, d.raw.clone()))
        .collect();
    let edited: Vec<NaiveDate> = days.iter().filter(|d| d.edited).map(|d| d.date).collect();
    // M7 — 주간 파싱 실패율(§7-1). 첫 실행부터 측정한다.
    tracing::info!("{}일 중 {}일 섹션 파싱 실패", days.len(), failures.len());

    let merged = weekly::merge(&days);
    let mut kpis = RangeKpis::default();
    for d in &days {
        kpis.add(&d.kpis);
    }

    let draft = weekly::render_wins(&merged.wins);
    let (wins_md, llm_used) = match summarizer {
        Some(s) if !draft.trim().is_empty() => {
            let out = s.summarize_raw(
                &template::weekly_system_prompt(),
                &weekly::merge_user_prompt(from, to, &draft),
            );
            match out {
                Some(m) if weekly::looks_like_wins(&m) => (m.trim().to_string(), true),
                Some(_) => {
                    tracing::warn!("주간 병합 응답이 형식을 벗어나 합쳐 둔 초안을 그대로 씁니다.");
                    (draft, false)
                }
                None => (draft, false),
            }
        }
        _ => (draft, false),
    };

    let markdown = template::compose_weekly(&template::WeeklyParts {
        from,
        to,
        wins_md: &wins_md,
        decisions: &merged.decisions,
        remaining: &merged.remaining,
        done: merged.done,
        kpis: &kpis,
        missing: &missing,
        parse_failures: &failures,
        edited: &edited,
        llm_used,
    });

    Ok(RangeDocument {
        markdown,
        days_included: included,
        days_missing: missing,
        parse_failures: failures.into_iter().map(|(d, _)| d).collect(),
        kpis,
        llm_used,
    })
}

// --------------------------------------------------------------------------- //
// 저장
// --------------------------------------------------------------------------- //

/// `targets` 로 지정된 곳(또는 config 에서 enabled 된 곳)에 저장.
pub fn save(cfg: &Config, worklog: &WorkLog, targets: Option<&[String]>) -> Vec<SinkResult> {
    let want: Option<Vec<&str>> = targets
        .filter(|t| !t.is_empty())
        .map(|t| t.iter().map(String::as_str).collect());
    let enabled = |name: &str, cfg_enabled: bool| match &want {
        Some(w) => w.contains(&name),
        None => cfg_enabled,
    };
    let mut sinks: Vec<Box<dyn Sink>> = Vec::new();
    if enabled("markdown", cfg.outputs.markdown.enabled) {
        sinks.push(Box::new(MarkdownSink::new(cfg.outputs.markdown.clone())));
    }
    if enabled("obsidian", cfg.outputs.obsidian.enabled) {
        sinks.push(Box::new(ObsidianSink::new(cfg.outputs.obsidian.clone())));
    }
    if enabled("notion", cfg.outputs.notion.enabled) {
        sinks.push(Box::new(NotionSink::new(cfg.outputs.notion.clone())));
    }
    sinks.iter().map(|s| s.write(worklog)).collect()
}

// --------------------------------------------------------------------------- //
// 히스토리 (저장된 markdown 파일 기준)
// --------------------------------------------------------------------------- //

fn is_date_str(s: &str) -> bool {
    s.len() == 10
        && s.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        })
}

pub fn list_history(cfg: &Config) -> Vec<NaiveDate> {
    let dir = cfg.outputs.markdown.resolved_dir();
    let mut dates: Vec<NaiveDate> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".md")
                && is_date_str(stem)
                && let Ok(d) = NaiveDate::parse_from_str(stem, "%Y-%m-%d")
                && !dates.contains(&d)
            {
                dates.push(d);
            }
        }
    }
    dates.sort_unstable_by(|a, b| b.cmp(a));
    dates
}

/// 저장된 마크다운 읽기. 경로 traversal 방지: `YYYY-MM-DD` 형식만 허용.
pub fn read_saved(cfg: &Config, date_str: &str) -> Option<String> {
    if !is_date_str(date_str) {
        return None;
    }
    let path = cfg
        .outputs
        .markdown
        .resolved_dir()
        .join(format!("{date_str}.md"));
    if !path.is_file() {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

// --------------------------------------------------------------------------- //
// 앱 UI 용 근거(evidence) 추출
// --------------------------------------------------------------------------- //

pub fn to_evidence(data: &DailyData, tz: Tz) -> Value {
    let session_ev = |s: &crate::model::Session| {
        json!({
            "project": s.project, "branch": s.git_branch,
            "title": s.title.clone().or_else(|| s.intent.clone()), "intent": s.intent,
            "files": s.files_edited.len(), "tokens": s.output_tokens,
            "tools": s.tool_counts,
        })
    };
    let git: Vec<Value> = data
        .git
        .iter()
        .flat_map(|g| g.commits.iter())
        .map(|c| {
            json!({
                "repo": c.repo, "hash": c.short_hash(), "subject": c.subject,
                "insertions": c.insertions, "deletions": c.deletions, "files": c.files_changed,
            })
        })
        .collect();
    // 칩 수·지표·본문과 일치하도록 자동요약(meta) 세션은 근거에서도 제외한다.
    let claude: Vec<Value> = data
        .claude
        .iter()
        .flat_map(|c| c.sessions.iter())
        .filter(|s| !is_meta_session(s))
        .map(session_ev)
        .collect();
    let codex: Vec<Value> = data
        .codex
        .iter()
        .flat_map(|c| c.sessions.iter())
        .filter(|s| !is_meta_session(s))
        .map(session_ev)
        .collect();
    let calendar: Vec<Value> = data
        .calendar
        .iter()
        .flat_map(|c| c.events.iter())
        .map(|e| {
            let when = if e.all_day {
                "종일".to_string()
            } else {
                let s = e.start.as_deref().and_then(|t| parse_iso_in(t, tz));
                let en = e.end.as_deref().and_then(|t| parse_iso_in(t, tz));
                format!("{}–{}", fmt_time(s.as_ref(), tz), fmt_time(en.as_ref(), tz))
            };
            json!({"title": e.title, "when": when, "location": e.location, "attendees": e.attendees.len()})
        })
        .collect();
    let notes: Vec<Value> = data
        .notes
        .iter()
        .map(|n| {
            json!({"id": n.id, "time": fmt_time(Some(&n.ts), tz), "text": n.text,
                   "tags": n.tags, "mentions": n.mentions, "source": n.source})
        })
        .collect();
    json!({"git": git, "claude": claude, "codex": codex, "calendar": calendar, "notes": notes})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::SourceState;
    use crate::model::{GitCommit, GitData, Session, SessionData};
    use chrono::Utc;
    use std::process::Command;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    // ---- 주간 모아보기(N8) ------------------------------------------------ //

    fn day_doc(date: NaiveDate, body: &str) -> crate::store::Document {
        crate::store::Document {
            date,
            summary_md: None,
            full_md: body.to_string(),
            generated_at: Utc::now(),
            edited_at: None,
            run_id: None,
            template: Some("standard".into()),
        }
    }

    /// 9/14(월)·9/15(화)·9/16(수) 문서 3장 + 9/17(목) 미생성.
    fn week_store() -> Store {
        let s = Store::open_in_memory().unwrap();
        s.document_put(&day_doc(
            d(2026, 9, 14),
            "# 업무일지 2026-09-14 (월)\n\n\
             ## 오늘의 성과\n- [alpha] 설계 초안 정리 (a1b2c3d)\n\n\
             ## 결정 · 요청 · 할 일\n- 결정: 인증은 OAuth\n- [ ] 타임아웃 10초로 조정\n\n\
             ## 지표\n- 커밋 **3** (+10/−2) · 저장소 1 · AI **2세션** · 회의 1건\n",
        ))
        .unwrap();
        s.document_put(&day_doc(
            d(2026, 9, 15),
            "# 업무일지 2026-09-15 (화)\n\n\
             ## 📌 오늘의 성과\n- [alpha] 설계 확정 (e4f5a6b)\n- 위키 정리\n\n\
             ## 결정 · 요청 · 할 일\n- [x] 타임아웃 10초로 조정 (e4f5a6b)\n- [ ] 리뷰 반영\n\n\
             ## 지표\n- 커밋 0 — 훑은 저장소 4개에서 내 작성자(1개)와 일치하는 커밋 없음 · AI **1세션**\n",
        ))
        .unwrap();
        // 사람이 손으로 갈아엎어 섹션이 사라진 문서.
        let broken = "# 업무일지 2026-09-16 (수)\n\n종일 회의. 나중에 정리.\n";
        s.document_put(&day_doc(d(2026, 9, 16), broken)).unwrap();
        s.document_mark_edited(d(2026, 9, 16), broken, None)
            .unwrap();
        s
    }

    #[test]
    fn generate_range_merges_days_and_never_writes() {
        let s = week_store();
        let cfg = Config::default();
        let (from, to) = (d(2026, 9, 14), d(2026, 9, 17));
        let r = generate_range(&cfg, &s, from, to, template::WEEKLY_TEMPLATE, None).unwrap();

        assert_eq!(
            r.days_included,
            vec![d(2026, 9, 14), d(2026, 9, 15), d(2026, 9, 16)]
        );
        assert_eq!(r.days_missing, vec![d(2026, 9, 17)]); // 목요일 — 침묵하지 않는다
        assert_eq!(r.parse_failures, vec![d(2026, 9, 16)]);
        assert!(!r.llm_used);
        assert_eq!(
            r.kpis,
            crate::weekly::RangeKpis {
                commits: 3,
                repos: 1,
                sessions: 3,
                meetings: 1
            }
        );

        let md = &r.markdown;
        assert!(md.starts_with("# 주간 업무일지 2026-09-14 ~ 09-17\n"));
        assert!(md.contains(
            "## 이번 주 성과\n**alpha**\n- 설계 확정 (e4f5a6b) (9/15)\n- 설계 초안 정리 (a1b2c3d) (9/14)"
        ));
        assert!(md.contains("**기타**\n- 위키 정리 (9/15)"));
        assert!(md.contains("## 결정 · 요청\n- 9/14 결정: 인증은 OAuth"));
        // 9/14 에 열렸다가 9/15 에 체크된 할 일은 빠지고, 9/15 것만 남는다.
        assert!(md.contains("## 남은 할 일 (1)\n- [ ] 리뷰 반영 (9/15 ~)"));
        assert!(md.contains("_이번 주 완료 1건_"));
        assert!(md.contains("## 지표 (주간 합계)\n- 커밋 3 · 저장소 1 · AI 3세션 · 회의 1건"));
        assert!(md.contains("⚠ 9/17 (목) 일지가 없어 빠졌습니다"));
        assert!(md.contains("⚠ 9/16 — 섹션을 찾지 못해 원문 그대로 포함"));
        assert!(md.contains("_(9/16 편집본 기준)_"));
        assert!(md.contains("_중복 병합(LLM)을 쓰지 않아"));
        assert!(md.contains("### 9/16 원문\n\n종일 회의. 나중에 정리."));

        // 저장소에는 아무것도 쓰지 않았다 — 문서 3장 그대로, 스키마 그대로.
        assert_eq!(s.document_statuses(from, to).unwrap().len(), 3);
        assert_eq!(s.schema_version().unwrap(), crate::store::SCHEMA_VERSION);
        assert_eq!(crate::store::SCHEMA_VERSION, 3);
    }

    #[test]
    fn generate_range_weekend_gaps_are_silent_and_bad_range_errors() {
        let s = Store::open_in_memory().unwrap();
        let cfg = Config::default();
        // 9/19(토)·9/20(일)만 비어 있는 구간 — 경고하지 않는다.
        let r = generate_range(
            &cfg,
            &s,
            d(2026, 9, 19),
            d(2026, 9, 20),
            template::WEEKLY_TEMPLATE,
            None,
        )
        .unwrap();
        assert!(r.days_missing.is_empty() && r.days_included.is_empty());
        assert!(!r.markdown.contains("⚠"));
        assert!(r.markdown.contains("## 이번 주 성과\n- (없음)"));
        assert!(r.markdown.contains("## 남은 할 일 (0)"));
        assert!(r.markdown.contains("- 기록된 지표 없음"));

        assert!(matches!(
            generate_range(
                &cfg,
                &s,
                d(2026, 9, 18),
                d(2026, 9, 14),
                template::WEEKLY_TEMPLATE,
                None
            ),
            Err(RangeError::BadRange { .. })
        ));
    }

    #[test]
    fn generate_range_uses_one_llm_call_and_falls_back_on_bad_output() {
        use crate::config::SummarizerConfig;
        use crate::summarize::LlmCaller;
        use std::sync::Mutex;

        struct Once(Mutex<Vec<String>>, &'static str);
        impl LlmCaller for Once {
            fn call(
                &self,
                system: &str,
                _user: &str,
                _cfg: &SummarizerConfig,
                _cancel: Option<&std::sync::atomic::AtomicBool>,
            ) -> Option<String> {
                self.0.lock().unwrap().push(system.to_string());
                Some(self.1.to_string())
            }
        }
        let cfg = Config::default();
        let scfg = SummarizerConfig {
            provider: "claude_cli".into(),
            ..Default::default()
        };
        let s = week_store();

        // (1) 형식을 지킨 응답 → 그대로 쓴다. 호출은 정확히 1회.
        let fake = Box::new(Once(
            Mutex::new(vec![]),
            "**alpha**\n- 설계 초안 정리·확정 (a1b2c3d) (9/14, 9/15)",
        ));
        let ptr: *const Once = &*fake;
        let sum = Summarizer::with_caller(scfg.clone(), fake);
        let r = generate_range(
            &cfg,
            &s,
            d(2026, 9, 14),
            d(2026, 9, 16),
            template::WEEKLY_TEMPLATE,
            Some(&sum),
        )
        .unwrap();
        assert!(r.llm_used);
        assert!(
            r.markdown
                .contains("- 설계 초안 정리·확정 (a1b2c3d) (9/14, 9/15)")
        );
        assert!(!r.markdown.contains("_중복 병합(LLM)을 쓰지 않아"));
        // SAFETY: caller 는 Summarizer 가 살아있는 동안 유효.
        let calls = unsafe { &*ptr }.0.lock().unwrap().clone();
        assert_eq!(calls, vec![template::weekly_system_prompt()]);

        // (2) 앵커가 사라진 응답 → 합쳐 둔 초안으로 되돌린다(지어낸 문서로 바꾸지 않는다).
        let sum =
            Summarizer::with_caller(scfg, Box::new(Once(Mutex::new(vec![]), "다 합쳤습니다.")));
        let r = generate_range(
            &cfg,
            &s,
            d(2026, 9, 14),
            d(2026, 9, 16),
            template::WEEKLY_TEMPLATE,
            Some(&sum),
        )
        .unwrap();
        assert!(!r.llm_used);
        assert!(r.markdown.contains("- 설계 확정 (e4f5a6b) (9/15)"));
        assert!(!r.markdown.contains("다 합쳤습니다."));
    }

    #[test]
    fn generated_document_uses_the_template_composer() {
        let tz = get_tz("Asia/Seoul");
        let a = crate::analyze::analyze(&crate::analyze::tests::sample(), tz);
        let md = crate::template::compose(
            "standard",
            d(2026, 7, 6),
            Some("> 오늘 한 일"),
            &a,
            tz,
            "# 원본 사실 데이터\n- 커밋 목록 등",
            false,
        );
        // 문서 제목은 요일까지, 이모지 없이.
        assert!(
            md.starts_with("# 업무일지 2026-07-06 (월)\n\n> 오늘 한 일\n\n## 지표\n- 커밋 **2**")
        );
        assert!(!md.contains("📝 업무일지") && !md.contains("핵심 성과"));
        assert!(
            !md.contains("수집 데이터 원본")
                && !md.contains("원본 사실 데이터")
                && !md.contains("<summary>수집")
        );
        assert!(!md.contains("수집 소스") && !md.contains("저장 대상"));
        assert!(md.ends_with("\n"));
    }

    #[test]
    fn enabled_sources_and_availability() {
        let mut cfg = Config::default();
        cfg.sources.codex.enabled = false;
        assert_eq!(enabled_sources(&cfg, None), vec!["git", "claude"]);
        // 요청 목록이 있으면 설정의 켜짐 여부와 무관하게 그 목록만(순서는 고정)
        let req = vec!["codex, naverworks".to_string(), "bogus".to_string()];
        assert_eq!(
            enabled_sources(&cfg, Some(&req)),
            vec!["codex", "naverworks"]
        );
        assert_eq!(enabled_sources(&cfg, Some(&[])), vec!["git", "claude"]);

        let statuses = vec![
            SourceStatus {
                name: "naverworks".into(),
                state: SourceState::Skipped,
                count: 0,
                note: None,
            },
            SourceStatus {
                name: "git".into(),
                state: SourceState::Ok,
                count: 4,
                note: None,
            },
            SourceStatus {
                name: "claude".into(),
                state: SourceState::Ok,
                count: 0,
                note: None,
            },
            SourceStatus {
                name: "codex".into(),
                state: SourceState::Error,
                count: 0,
                note: Some("x".into()),
            },
        ];
        cfg.outputs.obsidian.enabled = true;
        cfg.outputs.obsidian.vault_dir = "D:/v".into();
        let line = availability_line(&cfg, &statuses);
        assert_eq!(
            line,
            "수집 소스: Git ✅(4) · Claude ✅ · Codex ⚠️ · 캘린더(NaverWorks) ❌\n저장 대상: 로컬 md ✅ · Obsidian ✅ · Notion ❌"
        );
    }

    #[test]
    fn collect_reports_statuses_without_real_sources() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.sources.claude.projects_dir =
            dir.path().join("no-claude").to_string_lossy().into_owned();
        cfg.sources.codex.sessions_dir = dir.path().join("no-codex").to_string_lossy().into_owned();
        cfg.sources.git.scan_all_drives = false;
        cfg.sources.git.scan_roots = vec![];
        cfg.sources.naverworks.enabled = false;
        let ctx = make_context(&cfg, Some("2026-07-06")).unwrap();
        let c = collect(&cfg, &ctx, &enabled_sources(&cfg, None), vec![]);
        let by: HashMap<&str, &SourceStatus> =
            c.statuses.iter().map(|s| (s.name.as_str(), s)).collect();
        assert_eq!(
            c.statuses
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ALL_SOURCES
        );
        assert_eq!(by["git"].state, SourceState::Skipped);
        assert_eq!(by["claude"].state, SourceState::Skipped);
        assert_eq!(by["codex"].state, SourceState::Skipped);
        assert_eq!(by["naverworks"].state, SourceState::Disabled);
        assert!(c.data.is_empty());
        assert!(
            c.data
                .warnings
                .iter()
                .any(|w| w.starts_with("[claude] 건너뜀:"))
        );
        assert!(
            c.data
                .warnings
                .iter()
                .any(|w| w.starts_with("[git] 건너뜀:"))
        );
        assert!(make_context(&cfg, Some("bad")).is_err());

        // 요약 없이 생성하면 안내 문구 + 지표
        let r = generate(&cfg, Some("2026-07-06"), true, None).unwrap();
        assert!(
            r.worklog
                .full_markdown
                .contains("LLM 요약을 사용하지 않았습니다")
        );
        assert!(
            r.worklog
                .full_markdown
                .starts_with("# 업무일지 2026-07-06 (월)\n")
        );
        // 수집이 전부 비었으면 0 을 나열하지 않는다(N4).
        assert!(
            r.worklog
                .full_markdown
                .contains("## 지표\n- 기록된 지표 없음")
        );
        assert!(r.rendered.signal.is_empty() || r.rendered.signal.starts_with("가용 데이터"));

        // 템플릿을 골라 부르면 그 템플릿의 결정론적 부분으로 조합된다.
        let summarizer = Summarizer::new(cfg.summarizer.clone());
        let r = generate_with(
            &cfg,
            Some("2026-07-06"),
            true,
            None,
            &summarizer,
            None,
            Some("report"),
        )
        .unwrap();
        assert!(r.worklog.full_markdown.contains("## 지표"));
        assert!(!r.worklog.full_markdown.contains("<details>"));
    }

    #[test]
    fn history_and_read_saved_reject_traversal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("outside.md"), "SECRET").unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("2026-07-06.md"), "ok").unwrap();
        std::fs::write(logs.join("2026-07-08.md"), "later").unwrap();
        std::fs::write(logs.join("notes.md"), "x").unwrap();
        let mut cfg = Config::default();
        cfg.outputs.markdown.dir = logs.to_string_lossy().into_owned();

        assert_eq!(list_history(&cfg), vec![d(2026, 7, 8), d(2026, 7, 6)]);
        assert_eq!(read_saved(&cfg, "2026-07-06").as_deref(), Some("ok"));
        assert_eq!(read_saved(&cfg, "../outside"), None);
        assert_eq!(read_saved(&cfg, "..\\outside"), None);
        assert_eq!(read_saved(&cfg, "2026-07-06.md"), None);
        assert_eq!(read_saved(&cfg, "2026-07-07"), None);
    }

    #[test]
    fn save_targets_override_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.outputs.markdown.dir = dir.path().join("md").to_string_lossy().into_owned();
        cfg.outputs.markdown.enabled = false;
        let wl = WorkLog {
            target_date: d(2026, 7, 6),
            facts_markdown: String::new(),
            full_markdown: "본문".into(),
            data: DailyData::new(d(2026, 7, 6), "Asia/Seoul"),
            summary_markdown: None,
        };
        assert!(save(&cfg, &wl, None).is_empty()); // 켜진 출력 없음
        let res = save(&cfg, &wl, Some(&["markdown".to_string()]));
        assert_eq!(res.len(), 1);
        assert!(res[0].ok);
        assert!(dir.path().join("md").join("2026-07-06.md").exists());
        let res = save(&cfg, &wl, Some(&["obsidian".to_string()]));
        assert!(!res[0].ok); // vault 미설정
    }

    fn git_ok() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    #[test]
    fn disambiguates_same_named_repos() {
        if !git_ok() {
            return;
        }
        use crate::collect::git::common_dir_of;
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("aa").join("app");
        let b = dir.path().join("bb").join("app");
        let solo = dir.path().join("solo");
        for p in [&a, &b, &solo] {
            std::fs::create_dir_all(p).unwrap();
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(p)
                    .args(["init", "-q"])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let ka = common_dir_of(&a).unwrap().to_string_lossy().into_owned();
        let kb = common_dir_of(&b).unwrap().to_string_lossy().into_owned();
        let ks = common_dir_of(&solo).unwrap().to_string_lossy().into_owned();
        assert_ne!(ka, kb);
        let commit = |repo: &str, hash: &str, key: &str| GitCommit {
            repo: repo.into(),
            hash: hash.into(),
            author: "m".into(),
            when: Utc::now(),
            subject: "s".into(),
            files_changed: 0,
            insertions: 0,
            deletions: 0,
            repo_path: key.into(),
        };
        let mut data = DailyData::new(d(2026, 7, 6), "Asia/Seoul");
        data.git = Some(GitData {
            commits: vec![
                commit("app", "x", &ka),
                commit("app", "y", &kb),
                commit("solo", "z", &ks),
            ],
        });
        data.claude = Some(SessionData {
            sessions: vec![Session {
                session_id: Some("1".into()),
                project: Some("app".into()),
                cwd: Some(a.to_string_lossy().into_owned()),
                title: Some("t".into()),
                ..Default::default()
            }],
        });
        disambiguate_repo_names(&mut data);
        let repos: Vec<&str> = data
            .git
            .as_ref()
            .unwrap()
            .commits
            .iter()
            .map(|c| c.repo.as_str())
            .collect();
        assert_eq!(repos, vec!["aa/app", "bb/app", "solo"]);
        // 같은 물리 저장소(a)의 세션은 git 커밋과 같은 이름으로 매칭
        assert_eq!(
            data.claude.as_ref().unwrap().sessions[0].project.as_deref(),
            Some("aa/app")
        );
    }

    /// 한 세션이 파일 두 개로 쪼개져 들어와도 하나로 합쳐진다(구간·파일·도구·질답 병합).
    #[test]
    fn merges_one_session_split_across_files() {
        use crate::model::QaTurn;
        use crate::time::parse_iso;

        let dir = tempfile::tempdir().unwrap();
        let short = dir.path().join("wt-a").to_string_lossy().into_owned();
        let wide = dir.path().join("main").to_string_lossy().into_owned();
        let mut tools_a = IndexMap::new();
        tools_a.insert("Edit".to_string(), 3u32);
        let mut tools_b = IndexMap::new();
        tools_b.insert("Edit".to_string(), 2u32);
        tools_b.insert("Read".to_string(), 1u32);

        let mut data = DailyData::new(d(2026, 9, 16), "Asia/Seoul");
        data.claude = Some(SessionData {
            sessions: vec![
                // 짧은 조각(먼저 등장) — 제목 없음
                Session {
                    session_id: Some("S".into()),
                    project: Some("wt-a".into()),
                    cwd: Some(short.clone()),
                    intent: Some("첫 요청".into()),
                    files_edited: vec!["a.rs".into(), "b.rs".into()],
                    files_read: vec!["r.rs".into()],
                    commands: vec!["cargo test".into()],
                    tool_counts: tools_a,
                    output_tokens: 1_000,
                    first_ts: parse_iso("2026-09-16T01:00:00Z"),
                    last_ts: parse_iso("2026-09-16T02:00:00Z"),
                    qa: vec![QaTurn {
                        time: "10:00".into(),
                        question: "질문1".into(),
                        answer: "답1".into(),
                    }],
                    qa_dropped: 2,
                    ..Default::default()
                },
                // 넓은 조각 — 같은 session_id
                Session {
                    session_id: Some("S".into()),
                    project: Some("main".into()),
                    cwd: Some(wide.clone()),
                    git_branch: Some("main".into()),
                    title: Some("세션 제목".into()),
                    files_edited: vec!["b.rs".into(), "c.rs".into()],
                    commands: vec!["cargo test".into(), "cargo clippy".into()],
                    tool_counts: tools_b,
                    output_tokens: 4_000,
                    first_ts: parse_iso("2026-09-16T00:30:00Z"),
                    last_ts: parse_iso("2026-09-16T05:00:00Z"),
                    qa: vec![
                        QaTurn {
                            time: "10:00".into(),
                            question: "질문1".into(),
                            answer: "답1".into(),
                        },
                        QaTurn {
                            time: "13:00".into(),
                            question: "질문2".into(),
                            answer: "답2".into(),
                        },
                    ],
                    qa_dropped: 1,
                    ..Default::default()
                },
            ],
        });
        normalize_sessions(&mut data);

        let ss = &data.claude.as_ref().unwrap().sessions;
        assert_eq!(ss.len(), 1);
        let s = &ss[0];
        assert_eq!(s.session_id.as_deref(), Some("S"));
        assert_eq!(s.first_ts, parse_iso("2026-09-16T00:30:00Z")); // 두 조각의 합집합
        assert_eq!(s.last_ts, parse_iso("2026-09-16T05:00:00Z"));
        assert_eq!(s.cwd.as_deref(), Some(wide.as_str())); // 넓은 쪽의 cwd
        assert_eq!(s.git_branch.as_deref(), Some("main"));
        assert_eq!(s.title.as_deref(), Some("세션 제목")); // 비어 있지 않은 쪽
        assert_eq!(s.intent.as_deref(), Some("첫 요청")); // 먼저 채워진 쪽 유지
        assert_eq!(s.files_edited, vec!["a.rs", "b.rs", "c.rs"]); // 합집합
        assert_eq!(s.files_read, vec!["r.rs"]);
        assert_eq!(s.commands, vec!["cargo test", "cargo clippy"]);
        assert_eq!(s.tool_counts.get("Edit"), Some(&5)); // 도구는 합
        assert_eq!(s.tool_counts.get("Read"), Some(&1));
        assert_eq!(s.output_tokens, 4_000); // 같은 세션이므로 더하지 않고 최댓값
        assert_eq!(s.qa_dropped, 2);
        assert_eq!(
            s.qa.iter().map(|t| t.question.as_str()).collect::<Vec<_>>(),
            vec!["질문1", "질문2"] // (시각, 질문) 중복 제거
        );

        // 멱등: 다시 불러도 그대로
        let before = data.claude.clone();
        normalize_sessions(&mut data);
        assert_eq!(data.claude, before);
    }

    /// session_id 가 없는 세션은 (cwd, 시작 분)으로 묶인다. 분이 다르면 따로 남는다.
    #[test]
    fn sessions_without_id_group_by_cwd_and_start_minute() {
        use crate::time::parse_iso;
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        let at = |t: &str| Session {
            cwd: Some(cwd.clone()),
            project: Some("p".into()),
            first_ts: parse_iso(t),
            last_ts: parse_iso(t),
            ..Default::default()
        };
        let mut data = DailyData::new(d(2026, 9, 16), "Asia/Seoul");
        data.claude = Some(SessionData {
            sessions: vec![
                at("2026-09-16T01:00:10Z"),
                at("2026-09-16T01:00:50Z"), // 같은 분 → 병합
                at("2026-09-16T01:02:00Z"), // 다른 분 → 별도
            ],
        });
        normalize_sessions(&mut data);
        assert_eq!(data.claude.as_ref().unwrap().sessions.len(), 2);
    }

    /// 같은 저장소의 worktree 들에서 열린 세션은 한 프로젝트로 모인다.
    #[test]
    fn worktree_sessions_roll_up_to_one_project() {
        if !git_ok() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("agent-platform-backend");
        std::fs::create_dir_all(&main).unwrap();
        let g = |args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(&main).args(args).output();
            assert!(out.is_ok_and(|o| o.status.success()), "git {args:?}");
        };
        g(&["init", "-q"]);
        g(&["config", "user.email", "me@x"]);
        g(&["config", "user.name", "Me"]);
        g(&["config", "commit.gpgsign", "false"]);
        std::fs::write(main.join("a.txt"), "x\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "init"]);
        let wt1 = dir.path().join("wt-feature");
        let wt2 = dir.path().join("wt-hotfix");
        g(&["worktree", "add", "-q", wt1.to_str().unwrap(), "-b", "f1"]);
        g(&["worktree", "add", "-q", wt2.to_str().unwrap(), "-b", "f2"]);

        let sess = |id: &str, p: &Path| Session {
            session_id: Some(id.into()),
            cwd: Some(p.to_string_lossy().into_owned()),
            project: Some(
                p.file_name().unwrap().to_string_lossy().into_owned(), // 수집기가 붙인 worktree 폴더명
            ),
            first_ts: Some(Utc::now()),
            last_ts: Some(Utc::now()),
            ..Default::default()
        };
        let mut data = DailyData::new(d(2026, 9, 16), "Asia/Seoul");
        data.claude = Some(SessionData {
            sessions: vec![
                sess("a", &main),
                sess("b", &wt1),
                sess("c", &wt2),
                // 저장소가 아닌 cwd → 기존 프로젝트명 유지
                Session {
                    session_id: Some("d".into()),
                    cwd: Some(dir.path().to_string_lossy().into_owned()),
                    project: Some("그대로".into()),
                    ..Default::default()
                },
            ],
        });
        normalize_sessions(&mut data);
        let projects: Vec<&str> = data
            .claude
            .as_ref()
            .unwrap()
            .sessions
            .iter()
            .map(|s| s.project.as_deref().unwrap_or("?"))
            .collect();
        assert_eq!(projects.iter().filter(|p| **p == "그대로").count(), 1);
        let repo_rows: Vec<&&str> = projects
            .iter()
            .filter(|p| **p == "agent-platform-backend")
            .collect();
        assert_eq!(repo_rows.len(), 3); // 세션 3개가 모두 같은 프로젝트명으로

        // worktree 세션도 **물리 저장소 루트**(본체)를 들고 나온다 — 제외 글롭이
        // worktree 경로를 몰라도 같은 저장소로 알아보게 하는 근거(N0).
        let want = dunce::canonicalize(&main)
            .unwrap()
            .to_string_lossy()
            .to_lowercase();
        let all = &data.claude.as_ref().unwrap().sessions;
        for s in all
            .iter()
            .filter(|s| s.project.as_deref() == Some("agent-platform-backend"))
        {
            assert_eq!(
                s.repo_root.as_deref().map(str::to_lowercase),
                Some(want.clone()),
                "worktree 세션의 저장소 루트"
            );
        }
        // 저장소가 아닌 cwd 는 루트도 없다.
        let plain = all
            .iter()
            .find(|s| s.project.as_deref() == Some("그대로"))
            .unwrap();
        assert!(plain.repo_root.is_none());

        // 본체 경로 하나만 적어도 worktree 세션까지 제외에 걸린다 — 걸리는 근거는 루트뿐이다
        // (cwd 는 형제 폴더라 글롭 밖이고, 글롭이 경로라 프로젝트 이름으로는 걸리지 않는다).
        let ex = crate::exclude::Excluder::new(&[dunce::canonicalize(&main)
            .unwrap()
            .to_string_lossy()
            .into_owned()]);
        assert_eq!(all.iter().filter(|s| ex.matches_session(s)).count(), 3);
        for s in all.iter().filter(|s| {
            s.session_id
                .as_deref()
                .is_some_and(|id| id == "b" || id == "c")
        }) {
            assert!(
                !ex.matches(s.cwd.as_deref().unwrap()),
                "worktree cwd 는 글롭 밖"
            );
            assert!(ex.matches_session(s));
        }

        // 분석에서도 한 행으로 합쳐진다.
        let a = crate::analyze::analyze(&data, get_tz("Asia/Seoul"));
        let row = a
            .projects
            .iter()
            .find(|p| p.project == "agent-platform-backend")
            .expect("저장소 행");
        assert_eq!(row.sessions, 3);
    }

    #[test]
    fn evidence_shape() {
        let mut data = DailyData::new(d(2026, 7, 6), "Asia/Seoul");
        data.claude = Some(SessionData {
            sessions: vec![
                Session {
                    project: Some("p".into()),
                    title: Some("t".into()),
                    files_edited: vec!["a".into()],
                    ..Default::default()
                },
                Session {
                    intent: Some(format!("{} x", crate::render::WORKLOG_SENTINEL)),
                    ..Default::default()
                },
            ],
        });
        let ev = to_evidence(&data, get_tz("Asia/Seoul"));
        assert_eq!(ev["claude"].as_array().unwrap().len(), 1);
        assert_eq!(ev["claude"][0]["files"], 1);
        assert_eq!(ev["git"].as_array().unwrap().len(), 0);
        assert_eq!(short_hash("a").len(), 6);
        assert_ne!(short_hash("a"), short_hash("b"));
    }

    // --- 제외 목록 (§5-1 N0) --------------------------------------------- //

    /// 제외 대상 저장소 하나 + 평범한 저장소 하나가 섞인 하루.
    fn day_with_private_repo() -> DailyData {
        let ts = |s: &str| {
            Some(
                chrono::DateTime::parse_from_rfc3339(s)
                    .unwrap()
                    .with_timezone(&Utc),
            )
        };
        let mut data = DailyData::new(d(2026, 9, 18), "Asia/Seoul");
        data.claude = Some(SessionData {
            sessions: vec![
                Session {
                    session_id: Some("private".into()),
                    project: Some("a-corp-billing".into()),
                    cwd: Some(r"D:\works\a-corp\billing".into()),
                    title: Some("정산 마감 배치 오류".into()),
                    intent: Some("정산 마감이 왜 실패하는지 봐 줘".into()),
                    files_edited: vec![r"D:\works\a-corp\billing\src\settle.py".into()],
                    commands: vec!["pytest tests/settle".into()],
                    output_tokens: 900,
                    first_ts: ts("2026-09-18T01:00:00Z"),
                    last_ts: ts("2026-09-18T02:00:00Z"),
                    qa: vec![crate::model::QaTurn {
                        time: "10:03".into(),
                        question: "정산 테이블 스키마가 뭐야".into(),
                        answer: "settlement 테이블은 …".into(),
                    }],
                    ..Default::default()
                },
                Session {
                    session_id: Some("public".into()),
                    project: Some("worklog".into()),
                    cwd: Some(r"D:\study\Daily Work Log".into()),
                    title: Some("제외 목록 붙이기".into()),
                    files_edited: vec![r"D:\study\Daily Work Log\src\exclude.rs".into()],
                    output_tokens: 100,
                    first_ts: ts("2026-09-18T03:00:00Z"),
                    last_ts: ts("2026-09-18T04:00:00Z"),
                    ..Default::default()
                },
            ],
        });
        let commit = |repo: &str, path: &str, subject: &str, when: &str| GitCommit {
            repo: repo.into(),
            hash: "0123456789abcdef".into(),
            author: "me".into(),
            when: ts(when).unwrap(),
            subject: subject.into(),
            files_changed: 2,
            insertions: 40,
            deletions: 5,
            repo_path: path.into(),
        };
        data.git = Some(GitData {
            commits: vec![
                commit(
                    "a-corp-billing",
                    r"D:\works\a-corp\billing\.git",
                    "feat: 정산 마감 재시도",
                    "2026-09-18T02:10:00Z",
                ),
                commit(
                    "worklog",
                    r"D:\study\Daily Work Log\.git",
                    "feat: 제외 글롭",
                    "2026-09-18T04:10:00Z",
                ),
            ],
        });
        data
    }

    /// 걸린 저장소의 이름·경로·질답은 요약 프롬프트에 **0회** 나오고,
    /// 문서(사실 정리·지표)에는 `[비공개 프로젝트]` 집계로 남는다.
    #[test]
    fn excluded_repo_never_reaches_the_summary_signal() {
        let tz = get_tz("Asia/Seoul");
        let data = day_with_private_repo();
        let mut cfg = Config::default();

        // 제외 없이 만들면 신호에 그대로 실린다 — 대조군.
        let open = render_all(&cfg, &data, &[], tz);
        assert!(open.signal.contains("a-corp-billing"));
        assert!(open.signal.contains("정산 테이블 스키마가 뭐야"));

        cfg.sources.exclude = vec![r"D:\works\a-corp\**".into()];
        let r = render_all(&cfg, &data, &[], tz);
        for leak in [
            "a-corp",
            "billing",
            r"D:\works",
            "D:/works",
            "정산 마감 배치 오류",
            "정산 마감이 왜 실패하는지 봐 줘",
            "정산 테이블 스키마가 뭐야",
            "settlement",
            "settle.py",
            "정산 마감 재시도",
            "pytest tests/settle",
        ] {
            assert!(
                !r.signal.contains(leak),
                "요약 프롬프트에 제외 항목이 남았습니다: {leak:?}\n---\n{}",
                r.signal
            );
        }
        // 제외되지 않은 쪽은 그대로 나간다(하루가 통째로 비지 않는다).
        assert!(r.signal.contains("worklog"));
        assert!(r.signal.contains("제외 목록 붙이기"));

        // 문서: 이름은 사라지고 집계는 남는다.
        assert!(r.facts.contains(PRIVATE_PROJECT));
        assert!(!r.facts.contains("a-corp") && !r.facts.contains("정산"));
        assert!(!r.facts.contains("settle.py"));
        assert!(r.analysis_md.contains(PRIVATE_PROJECT));

        // 지표는 제외 전과 **한 숫자도 다르지 않다** — 이름만 가려질 뿐 하루가 줄지 않는다.
        let a = &r.analysis;
        let full = crate::analyze::analyze(&data, tz);
        assert_eq!(a.kpis, full.kpis);
        assert_eq!(a.kpis.commits, 2);
        assert_eq!(a.kpis.sessions, 2);
        assert_eq!(a.kpis.files_edited, 2);
        assert_eq!(a.kpis.tokens, 1000);
        let row = a
            .projects
            .iter()
            .find(|p| p.project == PRIVATE_PROJECT)
            .expect("[비공개 프로젝트] 행");
        let before = full
            .projects
            .iter()
            .find(|p| p.project == "a-corp-billing")
            .expect("제외 전 행");
        assert_eq!((row.sessions, row.commits, row.files), (1, 1, 1));
        assert_eq!(
            (row.minutes, row.insertions, row.deletions),
            (before.minutes, before.insertions, before.deletions)
        );
        assert_eq!(a.commit_types.get("기능").copied(), Some(2));
        assert!(a.projects.iter().all(|p| !p.project.contains("a-corp")));
        // 타임라인에도 이름이 남지 않는다.
        assert!(
            a.timeline
                .iter()
                .all(|e| !e.project.contains("a-corp") && !e.label.contains("정산"))
        );
    }

    /// 형제 worktree(경로가 글롭 밖) 와 경로를 잃은 같은 프로젝트 세션도 함께 빠진다.
    ///
    /// 실측(2026-09-16): `D:\study\ai-web-novel\**` 를 적었는데 그 저장소의 worktree가
    /// `C:\…\workspaces\…` 에 있어 세션 3개 중 2개가 요약 신호로 샜다. 이제 두 겹으로 막는다 —
    /// (1) 세션이 들고 있는 **물리 저장소 루트**, (2) 걸린 프로젝트 이름의 닫힘.
    #[test]
    fn excluded_repo_covers_worktrees_and_same_project_sessions() {
        let tz = get_tz("Asia/Seoul");
        let ts = |s: &str| {
            Some(
                chrono::DateTime::parse_from_rfc3339(s)
                    .unwrap()
                    .with_timezone(&Utc),
            )
        };
        let mut data = day_with_private_repo();
        let sessions = &mut data.claude.as_mut().unwrap().sessions;
        // (1) cwd 는 글롭 밖, 물리 저장소 루트만 안쪽.
        sessions.push(Session {
            session_id: Some("worktree".into()),
            project: Some("a-corp-billing".into()),
            cwd: Some(r"C:\Users\me\workspaces\wt-settle".into()),
            repo_root: Some(r"D:\works\a-corp\billing".into()),
            title: Some("정산 재시도 시나리오".into()),
            intent: Some("worktree 에서 정산 재시도 돌려 줘".into()),
            files_edited: vec![r"C:\Users\me\workspaces\wt-settle\src\retry.py".into()],
            commands: vec!["pytest tests/retry".into()],
            output_tokens: 300,
            first_ts: ts("2026-09-18T05:00:00Z"),
            last_ts: ts("2026-09-18T05:30:00Z"),
            ..Default::default()
        });
        // (2) cwd·루트 모두 글롭 밖 — 프로젝트 이름만 같다(루트 해석 실패).
        sessions.push(Session {
            session_id: Some("orphan".into()),
            project: Some("a-corp-billing".into()),
            cwd: Some(r"C:\Users\me\workspaces\wt-gone".into()),
            title: Some("정산 마감 로그 확인".into()),
            files_edited: vec![r"C:\Users\me\workspaces\wt-gone\src\audit.py".into()],
            output_tokens: 200,
            first_ts: ts("2026-09-18T06:00:00Z"),
            last_ts: ts("2026-09-18T06:20:00Z"),
            ..Default::default()
        });

        let mut cfg = Config::default();
        cfg.sources.exclude = vec![r"D:\works\a-corp\**".into()];
        let r = render_all(&cfg, &data, &[], tz);
        for leak in [
            "a-corp",
            "billing",
            "wt-settle",
            "wt-gone",
            "workspaces",
            "retry.py",
            "audit.py",
            "정산",
            "pytest tests/retry",
        ] {
            assert!(
                !r.signal.contains(leak),
                "요약 프롬프트에 제외 항목이 남았습니다: {leak:?}\n---\n{}",
                r.signal
            );
            assert!(
                !r.facts.contains(leak),
                "문서에 제외 항목이 남았습니다: {leak:?}"
            );
        }
        // 제외되지 않은 쪽은 그대로 나간다.
        assert!(r.signal.contains("worklog") && r.signal.contains("제외 목록 붙이기"));

        // 지표는 제외 전과 한 숫자도 다르지 않다 — 이름만 가려질 뿐 하루가 줄지 않는다.
        let full = crate::analyze::analyze(&data, tz);
        assert_eq!(r.analysis.kpis, full.kpis);
        assert_eq!(r.analysis.kpis.sessions, 4);
        assert_eq!(r.analysis.kpis.tokens, 1500);
        // 셋 다 [비공개 프로젝트] 한 행으로 접힌다.
        let row = r
            .analysis
            .projects
            .iter()
            .find(|p| p.project == PRIVATE_PROJECT)
            .expect("[비공개 프로젝트] 행");
        assert_eq!(row.sessions, 3);
        assert!(
            r.analysis
                .projects
                .iter()
                .all(|p| !p.project.contains("a-corp"))
        );
    }
}
