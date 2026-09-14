//! 오케스트레이션 서비스 — CLI 와 데스크톱 앱이 공유하는 핵심 로직.
//!
//! 수집기 실행 순서/결합, 요약, 문서 조합, 저장, 히스토리 조회를 한곳에 모아
//! `worklog-cli` 와 Tauri 앱이 동일하게 호출한다.

use std::{collections::HashMap, path::Path};

use chrono::NaiveDate;
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
    time::{TimeError, fmt_time, get_tz, parse_iso_in, resolve_day},
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
                GitCollector::new(cfg.sources.git.clone())
                    .with_extra_repos(cwds)
                    .collect(ctx)
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
                    .unwrap_or_else(|_| CollectorResult::fail("git", "예외: 수집 스레드 패닉"))
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

    if let Some(res) = git_res {
        absorb("git", &res, &mut data);
        statuses.insert("git", SourceStatus::from_result(&res, |d| d.commits.len()));
        data.git = res.data;
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

    disambiguate_repo_names(&mut data);
    Collected {
        data,
        statuses: statuses.into_values().collect(),
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

/// 최종 문서 조합: 제목 + 요약(없으면 안내) + 지표 + (선택) 원본 부록.
pub fn compose_full(
    target: NaiveDate,
    summary: Option<&str>,
    facts: &str,
    analysis_md: &str,
    include_raw: bool,
) -> String {
    let mut lines: Vec<String> = vec![format!("# 📝 업무일지 {target}"), String::new()];
    // (수집 소스·저장 대상 표기는 문서에 넣지 않음 — 앱 화면 칩으로만 표시)
    match summary {
        Some(s) => lines.push(s.trim().to_string()),
        None => lines.push("> LLM 요약을 사용하지 않았습니다. 아래 지표를 참고하세요.".into()),
    }
    if !analysis_md.trim().is_empty() {
        lines.push(String::new());
        lines.push(analysis_md.trim().to_string());
    }
    if include_raw {
        lines.extend(
            [
                "",
                "---",
                "",
                "<details>",
                "<summary>📊 수집 데이터 원본</summary>",
                "",
                facts.trim(),
                "",
                "</details>",
                "",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
    } else {
        lines.push(String::new());
    }
    lines.join("\n")
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

pub fn render_all(cfg: &Config, data: &DailyData, statuses: &[SourceStatus], tz: Tz) -> Rendered {
    let facts = render_facts(data, tz);
    let availability = availability_line(cfg, statuses);
    let analysis = analyze(data, tz);
    let analysis_md = render_analysis(&analysis);
    let mut signal = render_work_signal(
        data,
        tz,
        &format!("가용 데이터 — {}", availability.replace('\n', " / ")),
    );
    let tl = render_timeline_for_llm(&analysis);
    if !tl.is_empty() {
        signal = format!("{signal}\n{tl}");
    }
    let section = render_session_section(&render_session_blocks(data, tz, 8));
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
pub fn generate(
    cfg: &Config,
    date_spec: Option<&str>,
    no_llm: bool,
    sources: Option<&[String]>,
) -> Result<GenerateResult, TimeError> {
    let summarizer = Summarizer::new(cfg.summarizer.clone());
    let store = open_store();
    generate_with(cfg, date_spec, no_llm, sources, &summarizer, store.as_ref())
}

pub fn generate_with(
    cfg: &Config,
    date_spec: Option<&str>,
    no_llm: bool,
    sources: Option<&[String]>,
    summarizer: &Summarizer,
    store: Option<&Store>,
) -> Result<GenerateResult, TimeError> {
    let ctx = make_context(cfg, date_spec)?;
    let wanted = enabled_sources(cfg, sources);
    let note_items = notes::items_for(store, ctx.target_date());
    let Collected { data, statuses } = collect(cfg, &ctx, &wanted, note_items);
    let rendered = render_all(cfg, &data, &statuses, ctx.tz());
    let summary = if !no_llm && !data.is_empty() {
        summarizer.summarize_day(
            &rendered.signal,
            &ctx.target_date().to_string(),
            &rendered.availability,
        )
    } else {
        None
    };
    let full = compose_full(
        ctx.target_date(),
        summary.as_deref(),
        &rendered.facts,
        &rendered.analysis_md,
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

    #[test]
    fn compose_full_raw_toggle() {
        let md = compose_full(
            d(2026, 7, 6),
            Some("## 한 줄 요약\n오늘 한 일"),
            "# 원본 사실 데이터\n- 커밋 목록 등",
            "## 📊 오늘 지표\n- 커밋 3",
            false,
        );
        assert!(md.starts_with(
            "# 📝 업무일지 2026-07-06\n\n## 한 줄 요약\n오늘 한 일\n\n## 📊 오늘 지표\n- 커밋 3\n"
        ));
        assert!(
            !md.contains("수집 데이터 원본")
                && !md.contains("원본 사실 데이터")
                && !md.contains("<details>")
        );
        assert!(!md.contains("수집 소스") && !md.contains("저장 대상"));
        let md = compose_full(d(2026, 7, 6), Some("s"), "# 원본 사실 데이터", "", true);
        assert!(md.contains("<details>\n<summary>📊 수집 데이터 원본</summary>\n\n# 원본 사실 데이터\n\n</details>\n"));
        let md = compose_full(d(2026, 7, 6), None, "", "", false);
        assert!(md.contains("> LLM 요약을 사용하지 않았습니다. 아래 지표를 참고하세요."));
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
        assert!(r.worklog.full_markdown.contains("## 📊 오늘 지표"));
        assert!(r.rendered.signal.is_empty() || r.rendered.signal.starts_with("가용 데이터"));
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
}
