//! 주간 모아보기(N8) — 이미 만들어진 **일별 문서를 파싱해** 한 장으로 합친다.
//!
//! 여기 있는 함수는 전부 순수 함수다 — 입력은 문자열과 날짜뿐이고 DB·LLM·현재 시각을 만지지 않는다.
//! (DB 읽기와 LLM 1회 호출은 [`crate::service::generate_range`] 가 맡는다.)
//!
//! 파싱 대상은 모든 템플릿 공통인 두 섹션뿐이다 — [`template::T_WINS`] · [`template::T_DECISIONS`].
//! 제목은 이모지·공백을 턴 키로 비교한다(`apps/desktop/src/util.ts` 의 `headingKey` 와 같은 규칙) —
//! 사람이 손으로 고친 문서가 `## 📌 오늘의 성과` 처럼 돼 있어도 찾아낸다.
//!
//! 두 섹션을 **모두** 찾지 못한 문서는 조용히 빠뜨리지 않고 `parse_failed` 로 표시해 원문을 붙인다(원칙 5).
//! 단 그런 문서라도 **지표 줄은 따로 살려 합산한다** — [`find_metrics`] 는 제목 키에 `지표` 가 들어가면
//! 다 보기 때문에 v1 문서의 `## 📊 오늘 지표` 도 잡힌다. 합산 여부는 경고 문구에 그대로 적는다.

use std::{collections::HashMap, sync::OnceLock};

use chrono::NaiveDate;
use indexmap::IndexMap;
use regex::Regex;
use serde::Serialize;

use crate::{render::METRICS_EMPTY, template};

/// `[프로젝트]` 태그가 없는 성과 줄이 모이는 그룹 이름.
pub const OTHER_PROJECT: &str = "기타";

/// 비어 있는 섹션에 쓰는 한 줄.
pub const NONE_LINE: &str = "- (없음)";

// --------------------------------------------------------------------------- //
// 제목 · 섹션
// --------------------------------------------------------------------------- //

/// 제목 문자열에서 한글·영숫자만 남긴다. `"📊 지표"` → `"지표"`, `"결정 · 요청 · 할 일"` → `"결정요청할일"`.
pub fn normalize_heading(title: &str) -> String {
    title
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '\u{ac00}'..='\u{d7a3}'))
        .collect()
}

/// 제목 줄이면 [`normalize_heading`] 결과, 아니면 None.
pub fn heading_key(line: &str) -> Option<String> {
    let s = line.trim_start();
    let hashes = s.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &s[hashes..];
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    Some(normalize_heading(rest))
}

/// 제목 키가 `matches` 를 만족하는 **첫** 섹션의 본문(다음 제목 줄 직전까지).
fn section_body_by(md: &str, matches: impl Fn(&str) -> bool) -> Option<String> {
    let mut found = false;
    let mut out: Vec<&str> = Vec::new();
    for line in md.lines() {
        if let Some(key) = heading_key(line) {
            if found {
                break;
            }
            found = matches(&key);
            continue;
        }
        if found {
            out.push(line);
        }
    }
    found.then(|| out.join("\n").trim().to_string())
}

/// `title` 섹션의 본문(다음 제목 줄 직전까지). 섹션이 없으면 None.
pub fn section_body(md: &str, title: &str) -> Option<String> {
    let want = normalize_heading(title);
    if want.is_empty() {
        return None;
    }
    section_body_by(md, |k| k == want)
}

/// 불릿 표시를 뗀 나머지 — `- ` · `* ` · `1. ` · `1) `. 불릿이 아니면 None.
///
/// 번호는 세 자리까지만 인정한다 — `2026. 09. 18 회의록` 같은 날짜 줄을 불릿으로 오해하지 않게.
fn strip_marker(t: &str) -> Option<&str> {
    if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
        return Some(rest);
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if !(1..=3).contains(&digits) {
        return None;
    }
    let rest = &t[digits..];
    rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") "))
}

/// 섹션 본문의 불릿. `- (없음)` 은 버린다.
///
/// 번호 매기기(`1. ` · `1) `)도 불릿으로 센다 — 손으로 고친 문서가 번호 목록이라고 조용히
/// 통째로 빠지면 안 된다(원칙 5).
///
/// 들여쓴 하위 불릿은 **부모의 `[프로젝트]` 태그를 다시 붙여** 같은 그룹으로 평평하게 집어넣는다.
/// (하위 항목으로 따로 렌더하지 않고 평탄화하는 쪽을 골랐다 — 뒷단 [`split_project`]·[`checkbox`]
/// 와 [`merge`] 가 한 줄 단위 그대로 돌아 가장 단순하다.) 부모에 태그가 없으면 자기 글자만
/// 남는다 — 어차피 부모와 같은 [`OTHER_PROJECT`] 그룹이다.
fn bullets(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut parent: Option<String> = None;
    for line in body.lines() {
        let Some(rest) = strip_marker(line.trim()) else {
            continue;
        };
        let rest = rest.trim();
        if rest.is_empty() || rest == "(없음)" {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            match &parent {
                Some(tag) => out.push(format!("[{tag}] {rest}")),
                None => out.push(rest.to_string()),
            }
        } else {
            parent = leading_tag(rest).map(|(p, _)| p);
            out.push(rest.to_string());
        }
    }
    out
}

/// `# 업무일지 …` 제목 줄을 뺀 본문(원문 첨부용).
fn body_without_title(md: &str) -> String {
    let mut lines = md.lines().peekable();
    while let Some(l) = lines.peek() {
        if l.trim().is_empty() {
            lines.next();
            continue;
        }
        if heading_key(l).is_some() && l.trim_start().starts_with("# ") {
            lines.next();
        }
        break;
    }
    lines.collect::<Vec<_>>().join("\n").trim().to_string()
}

// --------------------------------------------------------------------------- //
// 지표 한 줄
// --------------------------------------------------------------------------- //

/// 주간 합계에 쓰는 네 가지 — [`crate::render::render_metrics_line`] 출력에서 뽑는다.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RangeKpis {
    pub commits: u32,
    pub repos: u32,
    pub sessions: u32,
    pub meetings: u32,
}

impl RangeKpis {
    pub fn add(&mut self, o: &RangeKpis) {
        self.commits += o.commits;
        self.repos += o.repos;
        self.sessions += o.sessions;
        self.meetings += o.meetings;
    }

    /// 0 인 항목은 감춘다(N4). 전부 0 이면 [`METRICS_EMPTY`].
    pub fn line(&self) -> String {
        let mut items: Vec<String> = Vec::new();
        if self.commits > 0 {
            items.push(format!("커밋 {}", self.commits));
        }
        if self.repos > 0 {
            items.push(format!("저장소 {}", self.repos));
        }
        if self.sessions > 0 {
            items.push(format!("AI {}세션", self.sessions));
        }
        if self.meetings > 0 {
            items.push(format!("회의 {}건", self.meetings));
        }
        if items.is_empty() {
            return METRICS_EMPTY.to_string();
        }
        format!("- {}", items.join(" · "))
    }
}

struct MetricRes {
    commits: Regex,
    repos: Regex,
    sessions: Regex,
    meetings: Regex,
}

fn metric_res() -> &'static MetricRes {
    static RES: OnceLock<MetricRes> = OnceLock::new();
    RES.get_or_init(|| MetricRes {
        // "커밋 2 (+403/−3)" · "커밋 0 — 훑은 저장소 6개에서 …"(N4) 둘 다 첫 숫자가 커밋 수다.
        commits: Regex::new(r"^커밋\s+(\d+)").expect("정적 정규식"),
        repos: Regex::new(r"^저장소\s+(\d+)").expect("정적 정규식"),
        sessions: Regex::new(r"^AI\s+(\d+)세션").expect("정적 정규식"),
        meetings: Regex::new(r"^회의\s+(\d+)건").expect("정적 정규식"),
    })
}

fn cap(re: &Regex, s: &str) -> Option<u32> {
    re.captures(s)?.get(1)?.as_str().parse().ok()
}

/// `## 지표` 한 줄에서 커밋 · 저장소 · AI 세션 · 회의 수를 뽑는다. 없는 항목은 0.
///
/// 제목은 키에 `지표` 가 들어가면 모두 본다 — `## 지표` · `## 📊 지표` · v1 의 `## 📊 오늘 지표`.
/// 성과·결정 섹션이 깨진 문서라도 지표 줄은 대개 살아 있어서, 이걸로 주간 합계에 넣는다.
///
/// 지표 섹션이나 지표 줄 자체가 없으면 None — 합계에서 빠졌다고 말해 줘야 한다(원칙 5).
pub fn find_metrics(md: &str) -> Option<RangeKpis> {
    let mut k = RangeKpis::default();
    let body = section_body_by(md, |key| key.contains("지표"))?;
    let line = body.lines().map(str::trim).find(|l| l.starts_with("- "))?;
    let re = metric_res();
    for item in line.trim_start_matches("- ").split('·') {
        // 굵게 표시(`커밋 **2**`)를 털어 낸다.
        let item = item.replace('*', "");
        let item = item.trim();
        if let Some(n) = cap(&re.commits, item) {
            k.commits = n;
        } else if let Some(n) = cap(&re.repos, item) {
            k.repos = n;
        } else if let Some(n) = cap(&re.sessions, item) {
            k.sessions = n;
        } else if let Some(n) = cap(&re.meetings, item) {
            k.meetings = n;
        }
    }
    Some(k)
}

/// [`find_metrics`] 결과. 지표를 못 찾으면 전부 0.
pub fn parse_metrics(md: &str) -> RangeKpis {
    find_metrics(md).unwrap_or_default()
}

// --------------------------------------------------------------------------- //
// 하루 문서 파싱
// --------------------------------------------------------------------------- //

/// 성과 줄 하나 — `[프로젝트]` 태그와 나머지 본문.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WinBullet {
    pub project: String,
    pub text: String,
}

/// 할 일 줄 하나 — `- [ ]` / `- [x]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskBullet {
    pub done: bool,
    pub text: String,
}

/// 하루 문서에서 뽑아낸 것 전부.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DayDoc {
    pub date: NaiveDate,
    /// 사용자가 손으로 고친 문서(편집본이 정본).
    pub edited: bool,
    pub wins: Vec<WinBullet>,
    /// 체크박스가 아닌 줄(결정 · 요청 · `[메모 · …]` 칩).
    pub decisions: Vec<String>,
    pub tasks: Vec<TaskBullet>,
    pub kpis: RangeKpis,
    /// 두 공통 섹션을 **모두** 찾지 못했다 — 원문을 그대로 붙인다.
    pub parse_failed: bool,
    /// 원문(제목 줄 제외).
    pub raw: String,
}

/// 맨 앞의 `[프로젝트] 내용` 태그를 뜯는다. 태그가 없으면 None.
///
/// 빈 태그(`[ ]`)·체크 표시(`[x]`)는 프로젝트가 아니다.
fn leading_tag(text: &str) -> Option<(String, String)> {
    let t = text.trim_start_matches(['*', ' ']);
    let rest = t.strip_prefix('[')?;
    let end = rest.find(']')?;
    let name = rest[..end].trim();
    let body = rest[end + 1..].trim_start_matches(['*', ' ']).trim();
    (!name.is_empty() && !body.is_empty() && !name.eq_ignore_ascii_case("x"))
        .then(|| (name.to_string(), body.to_string()))
}

/// `[프로젝트] 내용` → `("프로젝트", "내용")`. 태그가 없으면 [`OTHER_PROJECT`].
pub fn split_project(text: &str) -> (String, String) {
    leading_tag(text).unwrap_or_else(|| (OTHER_PROJECT.to_string(), text.trim().to_string()))
}

/// `[ ] 할 일` → `Some((false, "할 일"))`, `[x] …` → `Some((true, …))`. 체크박스가 아니면 None.
fn checkbox(text: &str) -> Option<(bool, String)> {
    let (done, rest) = match text.strip_prefix("[ ]") {
        Some(r) => (false, r),
        None => (
            true,
            text.strip_prefix("[x]")
                .or_else(|| text.strip_prefix("[X]"))?,
        ),
    };
    let rest = rest.trim();
    (!rest.is_empty()).then(|| (done, rest.to_string()))
}

/// 하루 문서 한 장을 뜯는다. 섹션을 못 찾아도 실패하지 않는다 — `parse_failed` 로 표시할 뿐.
///
/// 지표는 성과·결정 섹션과 **따로** 찾는다([`find_metrics`]) — 섹션이 깨진 날도 지표 줄이 살아
/// 있으면 `kpis` 에 담겨 주간 합계에 들어간다.
pub fn parse_day(date: NaiveDate, full_md: &str, edited: bool) -> DayDoc {
    let wins_body = section_body(full_md, template::T_WINS);
    let dec_body = section_body(full_md, template::T_DECISIONS);
    let parse_failed = wins_body.is_none() && dec_body.is_none();

    let wins = wins_body
        .as_deref()
        .map(bullets)
        .unwrap_or_default()
        .into_iter()
        .map(|t| {
            let (project, text) = split_project(&t);
            WinBullet { project, text }
        })
        .collect();

    let mut decisions: Vec<String> = Vec::new();
    let mut tasks: Vec<TaskBullet> = Vec::new();
    for b in dec_body.as_deref().map(bullets).unwrap_or_default() {
        match checkbox(&b) {
            Some((done, text)) => tasks.push(TaskBullet { done, text }),
            None => decisions.push(b),
        }
    }

    DayDoc {
        date,
        edited,
        wins,
        decisions,
        tasks,
        kpis: parse_metrics(full_md),
        parse_failed,
        raw: body_without_title(full_md),
    }
}

// --------------------------------------------------------------------------- //
// 합치기
// --------------------------------------------------------------------------- //

/// 한 프로젝트의 이번 주 성과.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WinGroup {
    pub project: String,
    /// `내용 (9/16)` — 날짜 역순.
    pub bullets: Vec<String>,
}

/// 여러 날을 합친 결과(결정론적 — LLM 이전 상태).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Merged {
    pub wins: Vec<WinGroup>,
    /// `9/16 결정: …` — 날짜 역순.
    pub decisions: Vec<String>,
    /// `- [ ] … (9/16 ~)` — 처음 나온 순서.
    pub remaining: Vec<String>,
    /// 체크됐거나 뒷날 완료된 할 일 수.
    pub done: usize,
}

/// `2026-09-16` → `9/16`.
pub fn fmt_md(d: NaiveDate) -> String {
    use chrono::Datelike;
    format!("{}/{}", d.month(), d.day())
}

/// 같은 할 일인지 비교할 키 — 꼬리의 근거 괄호 `(a1b2c3d)` · 칩 `[메모 · 할일]` 을 떼고 글자만 남긴다.
pub fn norm_task(text: &str) -> String {
    let mut s = text.trim();
    loop {
        let t = s.trim_end();
        let cut = match t.chars().last() {
            Some(')') => t.rfind('('),
            Some(']') => t.rfind('['),
            _ => None,
        };
        match cut {
            Some(i) if i > 0 => s = &t[..i],
            _ => {
                s = t;
                break;
            }
        }
    }
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '\u{ac00}'..='\u{d7a3}'))
        .flat_map(char::to_lowercase)
        .collect()
}

/// 날짜별 파싱 결과를 한 장 분량으로 합친다.
///
/// - 성과: `[프로젝트]` 로 묶고 그룹 안은 날짜 역순, 각 줄 끝에 `(M/D)`.
/// - 결정 · 요청: `M/D` 를 앞에 붙여 날짜 역순.
/// - 할 일: 같은 글자의 할 일이 **뒷날 체크되면** 남은 할 일에서 뺀다.
pub fn merge(days: &[DayDoc]) -> Merged {
    let mut by_date: Vec<&DayDoc> = days.iter().collect();
    by_date.sort_by_key(|d| d.date);

    // ---- 성과 ----
    let mut groups: IndexMap<String, (NaiveDate, Vec<(NaiveDate, String)>)> = IndexMap::new();
    for d in &by_date {
        for w in &d.wins {
            let e = groups
                .entry(w.project.clone())
                .or_insert_with(|| (d.date, Vec::new()));
            e.0 = e.0.max(d.date);
            e.1.push((d.date, format!("{} ({})", w.text, fmt_md(d.date))));
        }
    }
    let mut wins: Vec<(bool, NaiveDate, WinGroup)> = groups
        .into_iter()
        .map(|(project, (latest, mut items))| {
            // 최신 날짜 먼저(같은 날은 문서 순서 유지 — 안정 정렬).
            items.sort_by_key(|a| std::cmp::Reverse(a.0));
            (
                project == OTHER_PROJECT,
                latest,
                WinGroup {
                    project,
                    bullets: items.into_iter().map(|(_, t)| t).collect(),
                },
            )
        })
        .collect();
    // '기타' 는 항상 마지막, 나머지는 최근 활동 → 이름순.
    wins.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(b.1.cmp(&a.1))
            .then(a.2.project.cmp(&b.2.project))
    });

    // ---- 결정 · 요청 (날짜 역순) ----
    let mut decisions: Vec<String> = Vec::new();
    for d in by_date.iter().rev() {
        for text in &d.decisions {
            decisions.push(format!("{} {}", fmt_md(d.date), text));
        }
    }

    // ---- 할 일 이월 ----
    let mut first_open: IndexMap<String, (NaiveDate, String)> = IndexMap::new();
    let mut last_open: HashMap<String, NaiveDate> = HashMap::new();
    let mut last_done: HashMap<String, NaiveDate> = HashMap::new();
    for d in &by_date {
        for t in &d.tasks {
            let key = norm_task(&t.text);
            if key.is_empty() {
                continue;
            }
            if t.done {
                let e = last_done.entry(key).or_insert(d.date);
                *e = (*e).max(d.date);
            } else {
                let e = last_open.entry(key.clone()).or_insert(d.date);
                *e = (*e).max(d.date);
                first_open
                    .entry(key)
                    .or_insert_with(|| (d.date, t.text.clone()));
            }
        }
    }
    let mut remaining: Vec<String> = Vec::new();
    for (key, (since, text)) in &first_open {
        let open = last_open.get(key).copied();
        let done = last_done.get(key).copied();
        // 마지막으로 열려 있던 날이 마지막 체크된 날보다 뒤일 때만 남은 할 일.
        if open.is_some() && (done.is_none() || open > done) {
            remaining.push(format!("- [ ] {text} ({} ~)", fmt_md(*since)));
        }
    }
    let all: usize = {
        let mut keys: Vec<&String> = last_open.keys().chain(last_done.keys()).collect();
        keys.sort_unstable();
        keys.dedup();
        keys.len()
    };

    let done = all.saturating_sub(remaining.len());
    Merged {
        wins: wins.into_iter().map(|(_, _, g)| g).collect(),
        decisions,
        remaining,
        done,
    }
}

/// 성과 그룹 → `**프로젝트**` + 불릿 마크다운(LLM 입력이자 LLM 없을 때의 결과).
pub fn render_wins(groups: &[WinGroup]) -> String {
    let mut out: Vec<String> = Vec::new();
    for g in groups {
        if !out.is_empty() {
            out.push(String::new());
        }
        out.push(format!("**{}**", g.project));
        out.extend(g.bullets.iter().map(|b| format!("- {b}")));
    }
    out.join("\n")
}

/// 중복 병합 LLM 호출에 줄 user 프롬프트(system 은 [`template::weekly_system_prompt`]).
pub fn merge_user_prompt(from: NaiveDate, to: NaiveDate, wins_md: &str) -> String {
    format!(
        "기간: {from} ~ {to}\n\n\
         아래는 날짜별 일지에서 뽑아 프로젝트별로 묶은 '이번 주 성과' 초안이다. \
         같은 일을 가리키는 줄만 합치고, 형식은 그대로 유지해 본문만 출력하라.\n\n\
         ---\n{wins_md}\n---\n"
    )
}

fn anchor_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\(\d{1,2}/\d{1,2}").expect("정적 정규식"))
}

/// LLM 응답이 '성과 섹션' 꼴인지 — 불릿이 있고 `(M/D)` 앵커가 살아 있어야 한다.
/// 아니면 결정론적 병합본을 그대로 쓴다(지어낸 문서로 바꿔치지 않는다).
pub fn looks_like_wins(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty() && t.lines().any(|l| l.trim_start().starts_with("- ")) && anchor_re().is_match(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, m, day).unwrap()
    }

    const DOC_15: &str = "\
# 업무일지 2026-09-15 (화)

> 하루 한 줄

## 오늘의 성과
- [agent-platform] MCP 인증 방향 검토 (a1b2c3d)
- 사내 위키 정리 (wiki.md)

## 결정 · 요청 · 할 일
- 결정: 발송 판정을 색인 상태 기준으로 전환
- 요청(@김팀장): 결제 API 타임아웃 10초로  [메모 · 요청]
- [ ] 엑셀 청킹 max_tokens 재시도 루프 수정
- [ ] 리뷰 코멘트 반영

## 지표
- 커밋 **2** (+403/−3) · 저장소 1 · AI **4세션** · 출력 5K토큰 · 회의 2건 · 활동 09:00–12:00
";

    /// 이모지가 섞인 제목 + 편집으로 순서가 바뀐 문서.
    const DOC_16: &str = "\
# 업무일지 2026-09-16 (수)

## 📌 오늘의 성과
- [agent-platform] MCP 인증 방향 확정(OAuth+PAT) (e4f5a6b)

## ✅ 결정 · 요청 · 할 일
- [x] 엑셀 청킹 max_tokens 재시도 루프 수정 (e4f5a6b)
- [ ] 리뷰 코멘트 반영 [메모 · 할일]

## 📊 지표
- 커밋 0 — 훑은 저장소 6개에서 내 작성자(2개)와 일치하는 커밋 없음 · AI **3세션**
";

    /// 섹션이 깨진(손으로 갈아엎은) 문서.
    const DOC_BROKEN: &str = "\
# 업무일지 2026-09-17 (목)

오늘은 종일 회의였다. 정리는 나중에.
";

    /// v1 형식 문서 — 공통 두 섹션(`오늘의 성과`·`결정 · 요청 · 할 일`)은 없지만 지표 줄은 살아 있다.
    const DOC_V1: &str = "\
# 📝 업무일지 2026-09-17

## 오늘 한 일
- 결제 재시도 루프 손봄

## 📊 오늘 지표
- 커밋 **5** (+120/−8) · 저장소 2 · AI **1세션** · 회의 1건
";

    /// 번호 목록과 들여쓴 하위 불릿이 섞인(손으로 고친) 문서.
    const DOC_NESTED: &str = "\
# 업무일지 2026-09-18 (금)

## 오늘의 성과
1. [alpha] 인증 서버 배포 (a1b2c3d)
2) [beta] 결제 재시도 루프 수정 (e4f5a6b)
- [alpha] 배포 문서 정리 (deploy.md)
  - 롤백 절차 추가
  * 점검 항목 보강
- 사내 위키 정리
  - 링크 점검
2026. 09. 18 회의록 링크

## 결정 · 요청 · 할 일
1. 결정: 배포 창구 일원화
2. [ ] 릴리스 노트 작성
  - [ ] 스크린샷 첨부
";

    #[test]
    fn heading_keys_ignore_emoji_and_spacing() {
        assert_eq!(heading_key("## 오늘의 성과").as_deref(), Some("오늘의성과"));
        assert_eq!(
            heading_key("##   📌 오늘의  성과  ").as_deref(),
            Some("오늘의성과")
        );
        assert_eq!(
            heading_key("## 결정 · 요청 · 할 일").as_deref(),
            Some("결정요청할일")
        );
        assert_eq!(heading_key("### 9/16 원문").as_deref(), Some("916원문"));
        assert_eq!(heading_key("#태그"), None); // 공백 없는 건 제목이 아니다
        assert_eq!(heading_key("- 그냥 불릿"), None);
        assert_eq!(heading_key("####### 너무 깊음"), None);
        // template.rs 상수와 키가 일치한다.
        assert_eq!(normalize_heading(template::T_WINS), "오늘의성과");
        assert_eq!(normalize_heading(template::T_DECISIONS), "결정요청할일");
    }

    #[test]
    fn section_body_stops_at_next_heading() {
        let wins = section_body(DOC_15, template::T_WINS).unwrap();
        assert_eq!(
            wins,
            "- [agent-platform] MCP 인증 방향 검토 (a1b2c3d)\n- 사내 위키 정리 (wiki.md)"
        );
        // 이모지 제목도 같은 키로 찾는다.
        assert!(
            section_body(DOC_16, template::T_WINS)
                .unwrap()
                .contains("OAuth+PAT")
        );
        assert!(section_body(DOC_15, "없는 섹션").is_none());
        assert!(section_body(DOC_BROKEN, template::T_WINS).is_none());
    }

    #[test]
    fn parse_day_splits_wins_decisions_and_tasks() {
        let day = parse_day(d(9, 15), DOC_15, false);
        assert!(!day.parse_failed);
        assert_eq!(
            day.wins,
            vec![
                WinBullet {
                    project: "agent-platform".into(),
                    text: "MCP 인증 방향 검토 (a1b2c3d)".into()
                },
                WinBullet {
                    project: OTHER_PROJECT.into(),
                    text: "사내 위키 정리 (wiki.md)".into()
                },
            ]
        );
        assert_eq!(day.decisions.len(), 2);
        assert!(day.decisions[1].ends_with("[메모 · 요청]"));
        assert_eq!(day.tasks.len(), 2);
        assert!(day.tasks.iter().all(|t| !t.done));
        assert!(parse_day(d(9, 16), DOC_16, true).tasks[0].done);
    }

    #[test]
    fn numbered_items_are_bullets_too() {
        let day = parse_day(d(9, 18), DOC_NESTED, true);
        assert!(!day.parse_failed);
        // `1. ` · `2) ` 로 쓴 줄이 조용히 사라지지 않는다.
        assert_eq!(
            day.wins[0],
            WinBullet {
                project: "alpha".into(),
                text: "인증 서버 배포 (a1b2c3d)".into()
            }
        );
        assert_eq!(
            day.wins[1],
            WinBullet {
                project: "beta".into(),
                text: "결제 재시도 루프 수정 (e4f5a6b)".into()
            }
        );
        // 번호 매기기는 결정·할 일 섹션에서도 그대로 산다.
        assert_eq!(day.decisions, vec!["결정: 배포 창구 일원화"]);
        assert_eq!(
            day.tasks,
            vec![
                TaskBullet {
                    done: false,
                    text: "릴리스 노트 작성".into()
                },
                TaskBullet {
                    done: false,
                    text: "스크린샷 첨부".into()
                },
            ]
        );
        // 날짜처럼 보이는 줄(`2026. `)은 불릿이 아니다 — 네 자리 번호는 안 본다.
        assert!(day.wins.iter().all(|w| !w.text.contains("회의록")));
        assert_eq!(strip_marker("1. 하나"), Some("하나"));
        assert_eq!(strip_marker("12) 열둘"), Some("열둘"));
        assert_eq!(strip_marker("2026. 09. 18 회의록"), None);
        assert_eq!(strip_marker("1.붙여쓰기"), None);
        assert_eq!(strip_marker("그냥 문장"), None);
    }

    #[test]
    fn indented_sub_bullets_inherit_the_parent_project() {
        let day = parse_day(d(9, 18), DOC_NESTED, true);
        // 하위 불릿이 최상위로 승격돼 [프로젝트] 태그를 잃지 않는다 — 부모 그룹으로 평탄화된다.
        assert_eq!(
            day.wins
                .iter()
                .map(|w| (w.project.as_str(), w.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("alpha", "인증 서버 배포 (a1b2c3d)"),
                ("beta", "결제 재시도 루프 수정 (e4f5a6b)"),
                ("alpha", "배포 문서 정리 (deploy.md)"),
                ("alpha", "롤백 절차 추가"),
                ("alpha", "점검 항목 보강"),
                (OTHER_PROJECT, "사내 위키 정리"),
                (OTHER_PROJECT, "링크 점검"),
            ]
        );
        // 그래서 주간 성과도 프로젝트 세 덩어리로만 묶인다.
        let m = merge(&[day]);
        assert_eq!(
            m.wins
                .iter()
                .map(|g| (g.project.as_str(), g.bullets.len()))
                .collect::<Vec<_>>(),
            vec![("alpha", 4), ("beta", 1), (OTHER_PROJECT, 2)]
        );
        assert_eq!(
            m.wins[0].bullets[2], "롤백 절차 추가 (9/18)",
            "물려받은 태그는 본문에 남지 않는다"
        );
    }

    #[test]
    fn broken_document_is_flagged_not_dropped() {
        let day = parse_day(d(9, 17), DOC_BROKEN, true);
        assert!(day.parse_failed);
        assert!(day.wins.is_empty() && day.tasks.is_empty());
        // 제목 줄만 빠지고 원문은 그대로 남는다.
        assert_eq!(day.raw, "오늘은 종일 회의였다. 정리는 나중에.");
        // 한쪽 섹션만 있으면 실패가 아니다(회고용 템플릿엔 '오늘의 성과' 가 없다).
        let retro = "# 업무일지\n\n## 결정 · 요청 · 할 일\n- 결정: 보류";
        assert!(!parse_day(d(9, 18), retro, false).parse_failed);
    }

    #[test]
    fn metrics_line_parsing_handles_zero_hidden_and_commit_zero_forms() {
        let k = parse_metrics(DOC_15);
        assert_eq!(
            k,
            RangeKpis {
                commits: 2,
                repos: 1,
                sessions: 4,
                meetings: 2
            }
        );
        // N4 — 커밋 0 형태. "훑은 저장소 6개" 를 저장소 수로 오독하지 않는다.
        let k16 = parse_metrics(DOC_16);
        assert_eq!(
            k16,
            RangeKpis {
                commits: 0,
                repos: 0,
                sessions: 3,
                meetings: 0
            }
        );
        // 지표 자체가 없거나 비어 있는 문서.
        assert_eq!(parse_metrics(DOC_BROKEN), RangeKpis::default());
        assert_eq!(
            parse_metrics("## 지표\n- 기록된 지표 없음"),
            RangeKpis::default()
        );
        // '없다' 와 '0 이다' 를 구분한다 — 주간 경고 문구가 이걸로 갈린다.
        assert!(find_metrics(DOC_BROKEN).is_none());
        assert!(find_metrics("## 지표").is_none()); // 제목만 있고 지표 줄이 없음
        assert_eq!(
            find_metrics("## 지표\n- 기록된 지표 없음"),
            Some(RangeKpis::default())
        );
    }

    #[test]
    fn metrics_survive_a_parse_failed_day_including_the_v1_heading() {
        // 공통 두 섹션은 못 찾지만(파싱 실패) 지표 줄은 살아 있다 — 합계에서 조용히 빠지면 안 된다.
        let day = parse_day(d(9, 17), DOC_V1, false);
        assert!(day.parse_failed);
        assert!(day.wins.is_empty() && day.decisions.is_empty() && day.tasks.is_empty());
        let want = RangeKpis {
            commits: 5,
            repos: 2,
            sessions: 1,
            meetings: 1,
        };
        assert_eq!(day.kpis, want);
        assert_eq!(find_metrics(DOC_V1), Some(want));
        // 원문 첨부본(제목 줄 뺀 것)에서도 같은 값이 나온다 — compose_weekly 가 이걸로 판단한다.
        assert_eq!(find_metrics(&day.raw), Some(want));
        // 제목 변형 세 가지가 모두 같은 값.
        for heading in ["## 지표", "## 📊 지표", "## 📊 오늘 지표"] {
            assert_eq!(
                parse_metrics(&format!(
                    "{heading}\n- 커밋 5 · 저장소 2 · AI 1세션 · 회의 1건"
                )),
                want,
                "{heading}"
            );
        }
        // 그래서 주간 합계에도 들어간다.
        let mut sum = RangeKpis::default();
        for x in [parse_day(d(9, 15), DOC_15, false), day] {
            sum.add(&x.kpis);
        }
        assert_eq!(sum.line(), "- 커밋 7 · 저장소 3 · AI 5세션 · 회의 3건");
    }

    #[test]
    fn kpi_line_hides_zero_items() {
        let mut k = RangeKpis::default();
        k.add(&parse_metrics(DOC_15));
        k.add(&parse_metrics(DOC_16));
        assert_eq!(k.line(), "- 커밋 2 · 저장소 1 · AI 7세션 · 회의 2건");
        assert_eq!(RangeKpis::default().line(), METRICS_EMPTY);
        assert_eq!(
            RangeKpis {
                commits: 3,
                ..Default::default()
            }
            .line(),
            "- 커밋 3"
        );
    }

    #[test]
    fn merge_groups_wins_by_project_newest_first() {
        let m = merge(&[
            parse_day(d(9, 15), DOC_15, false),
            parse_day(d(9, 16), DOC_16, true),
        ]);
        assert_eq!(
            m.wins
                .iter()
                .map(|g| g.project.as_str())
                .collect::<Vec<_>>(),
            vec!["agent-platform", OTHER_PROJECT]
        );
        assert_eq!(
            m.wins[0].bullets,
            vec![
                "MCP 인증 방향 확정(OAuth+PAT) (e4f5a6b) (9/16)",
                "MCP 인증 방향 검토 (a1b2c3d) (9/15)",
            ]
        );
        assert_eq!(m.wins[1].bullets, vec!["사내 위키 정리 (wiki.md) (9/15)"]);
        assert_eq!(
            render_wins(&m.wins),
            "**agent-platform**\n\
             - MCP 인증 방향 확정(OAuth+PAT) (e4f5a6b) (9/16)\n\
             - MCP 인증 방향 검토 (a1b2c3d) (9/15)\n\
             \n\
             **기타**\n\
             - 사내 위키 정리 (wiki.md) (9/15)"
        );
    }

    #[test]
    fn decisions_are_prefixed_with_date_newest_first() {
        let m = merge(&[
            parse_day(d(9, 15), DOC_15, false),
            parse_day(d(9, 16), DOC_16, true),
        ]);
        assert_eq!(
            m.decisions,
            vec![
                "9/15 결정: 발송 판정을 색인 상태 기준으로 전환",
                "9/15 요청(@김팀장): 결제 API 타임아웃 10초로  [메모 · 요청]",
            ]
        );
    }

    #[test]
    fn todo_checked_on_a_later_day_is_not_remaining() {
        let m = merge(&[
            parse_day(d(9, 15), DOC_15, false),
            parse_day(d(9, 16), DOC_16, true),
        ]);
        // 9/15 에 열렸다가 9/16 에 체크된 항목은 빠지고, 9/16 에도 열려 있는 항목만 남는다.
        assert_eq!(m.remaining, vec!["- [ ] 리뷰 코멘트 반영 (9/15 ~)"]);
        assert_eq!(m.done, 1);
        // 근거 괄호·칩이 붙어도 같은 할 일로 본다.
        assert_eq!(
            norm_task("엑셀 청킹 max_tokens 재시도 루프 수정 (e4f5a6b)"),
            norm_task("엑셀 청킹 max_tokens 재시도 루프 수정")
        );
        assert_eq!(
            norm_task("리뷰 코멘트 반영 [메모 · 할일]"),
            norm_task("리뷰 코멘트 반영")
        );
        // 반대로 뒷날 다시 열리면 남는다.
        let reopened = merge(&[
            parse_day(
                d(9, 15),
                "## 결정 · 요청 · 할 일\n- [x] 리뷰 코멘트 반영",
                false,
            ),
            parse_day(
                d(9, 16),
                "## 결정 · 요청 · 할 일\n- [ ] 리뷰 코멘트 반영",
                false,
            ),
        ]);
        assert_eq!(reopened.remaining.len(), 1);
        assert_eq!(reopened.done, 0);
    }

    #[test]
    fn llm_output_guard() {
        assert!(looks_like_wins("**p**\n- 뭔가 했다 (9/16)"));
        assert!(!looks_like_wins("")); // 빈 응답
        assert!(!looks_like_wins("합칠 게 없습니다.")); // 불릿·앵커 없음
        assert!(!looks_like_wins("- 앵커가 사라진 줄")); // (M/D) 가 사라짐
    }

    #[test]
    fn merge_user_prompt_carries_range_and_draft() {
        let p = merge_user_prompt(d(9, 14), d(9, 18), "**p**\n- a (9/15)");
        assert!(p.starts_with("기간: 2026-09-14 ~ 2026-09-18"));
        assert!(p.contains("---\n**p**\n- a (9/15)\n---"));
    }

    #[test]
    fn empty_input_is_empty_output() {
        let m = merge(&[]);
        assert!(m.wins.is_empty() && m.decisions.is_empty() && m.remaining.is_empty());
        assert_eq!(m.done, 0);
        assert_eq!(render_wins(&m.wins), "");
        assert_eq!(fmt_md(d(9, 5)), "9/5");
    }
}
