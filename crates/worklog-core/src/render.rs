//! 수집한 DailyData 를 결정론적 Markdown(사실 정리)으로 변환.
//!
//! 결과물은 세 곳에 쓰인다.
//!   1) 요약기(LLM)의 입력 — 정제 신호(`render_work_signal`) + 시간순 이벤트 + 세션 질답 블록
//!   2) 최종 문서의 지표 섹션(`render_analysis`)
//!   3) 선택적 부록(수집 데이터 원본, `render_facts`)
//!
//! 문자열 하나하나가 v1(Python) 출력과 동일해야 골든 비교가 통과한다.

use chrono_tz::Tz;

use crate::{
    analyze::Analysis,
    collect::qa::squash_ws,
    model::{DailyData, Session},
    time::{fmt_time, human_duration, parse_iso_in},
};

// --------------------------------------------------------------------------- //
// 자동요약(meta) 세션 판별
// --------------------------------------------------------------------------- //

/// 요약기가 claude CLI 로 만든 세션에 심는 '안정적 표식'. 프롬프트 문구가 바뀌어도 불변.
pub const WORKLOG_SENTINEL: &str = "__WORKLOG_GENERATOR_AUTOSUMMARY__";

/// 표식이 없던 과거 요약 세션 대비 — 요약기 system 프롬프트의 '도입부'. '시작 일치'로만 판별한다
/// (부분일치는 이 도구를 만드는 진짜 세션까지 오삭제하므로).
const META_INTRO_SIGS: [&str; 5] = [
    "너는 하루치 개발 활동",
    "너는 개발자의 하루 활동",
    "하루치 개발 활동 데이터를",
    "하루치 개발 활동 로그",
    "개발자의 하루 활동 로그",
];

/// 이 도구가 만든 자동요약 세션인지.
pub fn is_meta_session(s: &Session) -> bool {
    let intent = s.intent.as_deref().unwrap_or("");
    let title = s.title.as_deref().unwrap_or("");
    if intent.contains(WORKLOG_SENTINEL) || title.contains(WORKLOG_SENTINEL) {
        return true;
    }
    let intent = intent.trim_start();
    META_INTRO_SIGS.iter().any(|sig| intent.starts_with(sig))
}

/// `render_session_section` 이 붙이는 세션 질답 섹션의 머리글(map-reduce 파싱 기준).
pub const SESSION_SECTION_HEADER: &str = "## Claude Code 세션 (질답 흐름)";

// --------------------------------------------------------------------------- //
// 헬퍼
// --------------------------------------------------------------------------- //

/// 천 단위 구분(`{:,}`).
pub fn fmt_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn base_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn non_meta(sessions: &[Session]) -> Vec<&Session> {
    sessions.iter().filter(|s| !is_meta_session(s)).collect()
}

fn session_head(s: &Session) -> String {
    s.title
        .as_deref()
        .filter(|t| !t.is_empty())
        .or(s.intent.as_deref().filter(|i| !i.is_empty()))
        .unwrap_or("(제목 없음)")
        .to_string()
}

// --------------------------------------------------------------------------- //
// 사실 정리 (부록)
// --------------------------------------------------------------------------- //

pub fn render_facts(data: &DailyData, tz: Tz) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "# {} 업무 데이터 ({})",
        data.target_date, data.tz_name
    ));
    lines.push(String::new());

    facts_calendar(&mut lines, data, tz);
    facts_git(&mut lines, data);
    facts_sessions(
        &mut lines,
        "## 🤖 Claude Code 작업",
        data.claude.as_ref().map(|c| c.sessions.as_slice()),
        true,
    );
    facts_sessions(
        &mut lines,
        "## 🧩 Codex 작업",
        data.codex.as_ref().map(|c| c.sessions.as_slice()),
        false,
    );
    facts_notes(&mut lines, data, tz);

    if !data.warnings.is_empty() {
        lines.push("## ⚠️ 수집 경고".into());
        for w in &data.warnings {
            lines.push(format!("- {w}"));
        }
        lines.push(String::new());
    }

    format!("{}\n", lines.join("\n").trim_end())
}

fn facts_calendar(lines: &mut Vec<String>, data: &DailyData, tz: Tz) {
    lines.push("## 📅 캘린더 일정 (NaverWorks)".into());
    let events = data
        .calendar
        .as_ref()
        .map(|c| c.events.as_slice())
        .unwrap_or(&[]);
    if events.is_empty() {
        lines.push("- (일정 없음 또는 미연동)".into());
        lines.push(String::new());
        return;
    }
    for ev in events {
        let title = ev
            .title
            .as_deref()
            .filter(|t| !t.is_empty())
            .unwrap_or("(제목 없음)");
        let when = if ev.all_day {
            "종일".to_string()
        } else {
            let s = ev.start.as_deref().and_then(|t| parse_iso_in(t, tz));
            let e = ev.end.as_deref().and_then(|t| parse_iso_in(t, tz));
            format!("{}–{}", fmt_time(s.as_ref(), tz), fmt_time(e.as_ref(), tz))
        };
        let mut extra = Vec::new();
        if let Some(loc) = ev.location.as_deref().filter(|l| !l.is_empty()) {
            extra.push(format!("@{loc}"));
        }
        if !ev.attendees.is_empty() {
            extra.push(format!("참석 {}명", ev.attendees.len()));
        }
        let suffix = if extra.is_empty() {
            String::new()
        } else {
            format!(" ({})", extra.join(", "))
        };
        lines.push(format!("- **{when}** {title}{suffix}"));
    }
    lines.push(String::new());
}

/// 사용자가 직접 남긴 메모. 없으면 섹션을 만들지 않는다(v1 출력과 동일하게).
fn facts_notes(lines: &mut Vec<String>, data: &DailyData, tz: Tz) {
    if data.notes.is_empty() {
        return;
    }
    lines.push("## 📝 메모".into());
    for n in &data.notes {
        lines.push(format!(
            "- **{}** {}{}",
            fmt_time(Some(&n.ts), tz),
            n.text,
            crate::notes::trailer(n)
        ));
    }
    lines.push(String::new());
}

fn facts_git(lines: &mut Vec<String>, data: &DailyData) {
    lines.push("## 💾 Git 커밋".into());
    let Some(git) = data.git.as_ref().filter(|g| !g.commits.is_empty()) else {
        lines.push("- (커밋 없음)".into());
        lines.push(String::new());
        return;
    };
    let total_ins: u64 = git.commits.iter().map(|c| c.insertions as u64).sum();
    let total_del: u64 = git.commits.iter().map(|c| c.deletions as u64).sum();
    lines.push(format!(
        "- 총 **{}커밋** · {}개 저장소 · +{}/-{} 줄",
        git.commits.len(),
        git.repos().len(),
        total_ins,
        total_del
    ));
    for repo in git.repos() {
        lines.push(format!("- **{repo}**"));
        for c in git.commits_for(repo) {
            lines.push(format!(
                "    - `{}` {} (+{}/-{}, {}파일)",
                c.short_hash(),
                c.subject,
                c.insertions,
                c.deletions,
                c.files_changed
            ));
        }
    }
    lines.push(String::new());
}

/// 세션 목록(Claude·Codex 공통)을 사실 정리 마크다운으로. `always` 가 false 면(Codex) 비었을 때 섹션 생략.
fn facts_sessions(
    lines: &mut Vec<String>,
    heading: &str,
    sessions: Option<&[Session]>,
    always: bool,
) {
    let sessions = non_meta(sessions.unwrap_or(&[]));
    if sessions.is_empty() {
        if always {
            lines.push(heading.into());
            lines.push("- (세션 없음)".into());
            lines.push(String::new());
        }
        return;
    }
    lines.push(heading.into());
    let total_tokens: u64 = sessions.iter().map(|s| s.output_tokens).sum();
    lines.push(format!(
        "- 총 **{}세션** · 출력 {} 토큰",
        sessions.len(),
        fmt_thousands(total_tokens)
    ));
    for s in sessions {
        let head = session_head(s);
        let proj = s
            .project
            .as_deref()
            .filter(|p| !p.is_empty())
            .or(s.cwd.as_deref().filter(|c| !c.is_empty()))
            .unwrap_or("?");
        let branch = s
            .git_branch
            .as_deref()
            .filter(|b| !b.is_empty())
            .map(|b| format!(" [{b}]"))
            .unwrap_or_default();
        lines.push(format!("- **{proj}**{branch}: {head}"));
        if let Some(intent) = s.intent.as_deref().filter(|i| !i.is_empty())
            && intent != head
        {
            lines.push(format!("    - 요청: {intent}"));
        }
        if !s.tool_counts.is_empty() {
            let mut tools: Vec<(&String, &u32)> = s.tool_counts.iter().collect();
            tools.sort_by(|a, b| b.1.cmp(a.1));
            let joined = tools
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!("    - 도구: {joined}"));
        }
        if !s.files_edited.is_empty() {
            let shown: Vec<&str> = s
                .files_edited
                .iter()
                .take(8)
                .map(|p| base_name(p))
                .collect();
            let more = if s.files_edited.len() > shown.len() {
                format!(" 외 {}개", s.files_edited.len() - shown.len())
            } else {
                String::new()
            };
            lines.push(format!(
                "    - 수정 파일({}): {}{more}",
                s.files_edited.len(),
                shown.join(", ")
            ));
        }
        for cmd in s.commands.iter().take(5) {
            lines.push(format!("    - `$ {cmd}`"));
        }
    }
    lines.push(String::new());
}

// --------------------------------------------------------------------------- //
// 분석 섹션 (핵심 성과 · 지표 · 프로젝트 집중 · 타임라인)
// --------------------------------------------------------------------------- //

fn tok(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M토큰", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}K토큰", n as f64 / 1_000.0)
    } else {
        format!("{n}토큰")
    }
}

/// `Analysis` → 마크다운. LLM 요약 아래에 붙는 사실 지표 섹션.
pub fn render_analysis(a: &Analysis) -> String {
    let k = &a.kpis;
    let mut lines: Vec<String> = Vec::new();

    if !a.highlights.is_empty() {
        lines.push("## ⭐ 핵심 성과".into());
        for h in &a.highlights {
            lines.push(format!("- {h}"));
        }
        lines.push(String::new());
    }

    lines.push("## 📊 오늘 지표".into());
    let span = match (&k.span_start, &k.span_end) {
        (Some(s), Some(e)) => format!(" · 활동 {s}–{e}"),
        _ => String::new(),
    };
    let mtg = if k.meetings > 0 {
        format!(" · 회의 {}건", k.meetings)
    } else {
        String::new()
    };
    let notes = if k.notes > 0 {
        format!(" · 메모 {}건", k.notes)
    } else {
        String::new()
    };
    lines.push(format!(
        "- 커밋 **{}** (+{}/−{}) · 저장소 {} · AI **{}세션** · 출력 {}{mtg}{notes}{span}",
        k.commits,
        fmt_thousands(k.insertions),
        fmt_thousands(k.deletions),
        k.repos,
        k.sessions,
        tok(k.tokens)
    ));
    if !a.commit_types.is_empty() {
        let mut items: Vec<(&String, &u32)> = a.commit_types.iter().collect();
        items.sort_by(|x, y| y.1.cmp(x.1));
        let dist = items
            .iter()
            .map(|(l, n)| format!("{l} {n}"))
            .collect::<Vec<_>>()
            .join(" · ");
        lines.push(format!("- 커밋 타입: {dist}"));
    }
    if !a.work_style.is_empty() {
        let mut items: Vec<(&String, &u32)> = a.tool_profile.iter().collect();
        items.sort_by(|x, y| y.1.cmp(x.1));
        let prof = items
            .iter()
            .take(5)
            .map(|(t, n)| format!("{t} {n}"))
            .collect::<Vec<_>>()
            .join(" · ");
        lines.push(format!("- 작업 성격: **{}** ({prof})", a.work_style));
    }
    lines.push(String::new());

    if !a.projects.is_empty() {
        lines.push("### 프로젝트별 집중".into());
        lines.push("| 프로젝트 | 집중시간 | 세션 | 파일 | 커밋 | 변경 |".into());
        lines.push("|---|--:|--:|--:|--:|--:|".into());
        for p in &a.projects {
            let dur = if p.minutes > 0 {
                human_duration(p.minutes * 60)
            } else {
                "–".into()
            };
            let chg = if p.commits > 0 {
                format!(
                    "+{}/−{}",
                    fmt_thousands(p.insertions),
                    fmt_thousands(p.deletions)
                )
            } else {
                "–".into()
            };
            lines.push(format!(
                "| {} | {dur} | {} | {} | {} | {chg} |",
                p.project, p.sessions, p.files, p.commits
            ));
        }
        lines.push(String::new());
    }

    if !a.timeline.is_empty() {
        lines.push("## 🕐 타임라인".into());
        for e in &a.timeline {
            let when = match &e.end {
                Some(end) => format!("{}–{end}", e.start),
                None => e.start.clone(),
            };
            match e.kind.as_str() {
                "commit" => {
                    let tag = e
                        .ctype
                        .as_deref()
                        .map(|c| format!("[{c}] "))
                        .unwrap_or_default();
                    lines.push(format!("- `{when}` 💾 {tag}{} · {}", e.label, e.project));
                }
                "meeting" => lines.push(format!("- `{when}` 📅 {}", e.label)),
                "note" => lines.push(format!("- `{when}` 📝 {}", e.label)),
                _ => lines.push(format!("- `{when}` 🤖 {} · {}", e.label, e.project)),
            }
        }
        lines.push(String::new());
    }

    format!("{}\n", lines.join("\n").trim_end())
}

/// 요약기에 넘길 '시간순 이벤트' 텍스트. LLM 이 시간대별 업무 서술에 쓴다.
pub fn render_timeline_for_llm(a: &Analysis) -> String {
    if a.timeline.is_empty() {
        return String::new();
    }
    let mut lines = vec!["## 시간순 이벤트 (이 순서로 '시간대별 업무'를 서술)".to_string()];
    for e in &a.timeline {
        let when = match &e.end {
            Some(end) => format!("{}–{end}", e.start),
            None => e.start.clone(),
        };
        let lbl = match e.kind.as_str() {
            "session" => "작업",
            "commit" => "커밋",
            "meeting" => "회의",
            "note" => "메모",
            other => other,
        };
        let proj = if e.kind == "meeting" || e.kind == "note" {
            String::new()
        } else {
            format!(" · {}", e.project)
        };
        lines.push(format!("- {when} [{lbl}] {}{proj}", e.label));
    }
    format!("{}\n", lines.join("\n"))
}

// --------------------------------------------------------------------------- //
// LLM 요약용 '정제 신호'
// --------------------------------------------------------------------------- //

/// 세션 제목을 프로젝트별로 묶어(중복 제거) 정제 신호에 추가. Claude·Codex 공통.
fn emit_session_titles(lines: &mut Vec<String>, heading: &str, sessions: &[Session]) {
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut by_proj: Vec<(String, Vec<(String, usize)>)> = Vec::new();
    for s in sessions {
        if is_meta_session(s) {
            continue;
        }
        let title = s
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                s.intent
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .chars()
                    .take(60)
                    .collect()
            });
        if title.is_empty() {
            continue;
        }
        let proj = s.project.clone().unwrap_or_else(|| "?".into());
        let key = (proj.clone(), title.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        match by_proj.iter_mut().find(|(p, _)| *p == proj) {
            Some((_, items)) => items.push((title, s.files_edited.len())),
            None => by_proj.push((proj, vec![(title, s.files_edited.len())])),
        }
    }
    if by_proj.is_empty() {
        return;
    }
    lines.push(heading.into());
    for (proj, items) in by_proj {
        let joined = items
            .iter()
            .map(|(t, n)| {
                if *n > 0 {
                    format!("{t}({n}파일)")
                } else {
                    t.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        lines.push(format!("- **{proj}**: {joined}"));
    }
    lines.push(String::new());
}

/// 요약기에 넣을 정제된 신호. 커밋 제목 + 세션 제목 중심, 명령어/원문 제외.
pub fn render_work_signal(data: &DailyData, tz: Tz, header: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    if !header.is_empty() {
        lines.push(header.into());
        lines.push(String::new());
    }

    if let Some(git) = data.git.as_ref().filter(|g| !g.commits.is_empty()) {
        lines.push("## Git 커밋 (완료된 결과물 · 1차 근거)".into());
        for repo in git.repos() {
            let subs: Vec<&str> = git.commits_for(repo).map(|c| c.subject.as_str()).collect();
            lines.push(format!("- **{repo}**: {}", subs.join("; ")));
        }
        lines.push(String::new());
    }

    if let Some(c) = data.claude.as_ref().filter(|c| !c.sessions.is_empty()) {
        emit_session_titles(
            &mut lines,
            "## Claude Code 작업 (세션 제목 기준)",
            &c.sessions,
        );
    }
    if let Some(c) = data.codex.as_ref().filter(|c| !c.sessions.is_empty()) {
        emit_session_titles(&mut lines, "## Codex 작업 (세션 제목 기준)", &c.sessions);
    }

    if let Some(cal) = data.calendar.as_ref().filter(|c| !c.events.is_empty()) {
        lines.push("## 일정".into());
        for e in &cal.events {
            let when = if e.all_day {
                "종일".to_string()
            } else {
                let s = e.start.as_deref().and_then(|t| parse_iso_in(t, tz));
                let en = e.end.as_deref().and_then(|t| parse_iso_in(t, tz));
                format!("{}-{}", fmt_time(s.as_ref(), tz), fmt_time(en.as_ref(), tz))
            };
            let loc = e
                .location
                .as_deref()
                .filter(|l| !l.is_empty())
                .map(|l| format!(" @{l}"))
                .unwrap_or_default();
            lines.push(format!(
                "- {when} {}{loc}",
                e.title.as_deref().unwrap_or("")
            ));
        }
        lines.push(String::new());
    }

    if !data.notes.is_empty() {
        lines.push("## 메모 (사용자가 직접 남긴 1차 사실 · 구두 요청·결정·할 일)".into());
        for n in &data.notes {
            lines.push(format!(
                "- {} {}{}",
                fmt_time(Some(&n.ts), tz),
                squash_ws(&n.text),
                crate::notes::trailer(n)
            ));
        }
        lines.push(String::new());
    }

    let body = lines.join("\n");
    let body = body.trim();
    if body.is_empty() {
        String::new()
    } else {
        format!("{body}\n")
    }
}

/// 요약(단일/맵리듀스)용: 비-meta 세션마다 (라벨, 질답 블록).
///
/// 각 블록 = 세션 제목 + 시간대 + 질답 흐름('시:분 Q: … → A: …') + 수정 파일.
pub fn render_session_blocks(data: &DailyData, tz: Tz, max_files: usize) -> Vec<(String, String)> {
    let mut blocks = Vec::new();
    for s in data.all_sessions() {
        if is_meta_session(s) {
            continue;
        }
        let proj = s.project.clone().unwrap_or_else(|| "?".into());
        let tag = if s.agent == crate::model::Agent::Codex {
            " · Codex"
        } else {
            ""
        };
        // 제목/의도는 원본 사용자 텍스트라 개행을 품을 수 있다. 구조선(### 헤더, 요청)에 그대로 넣으면
        // map-reduce 파서의 블록 구분자 "\n\n### " 를 주입해 오분할되므로 접는다.
        let title = s
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                s.intent
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .chars()
                    .take(60)
                    .collect()
            });
        let title = if title.is_empty() {
            "(제목 없음)".to_string()
        } else {
            squash_ws(&title)
        };
        let span = match (s.first_ts, s.last_ts) {
            (Some(f), Some(l)) => format!(" {}–{}", fmt_time(Some(&f), tz), fmt_time(Some(&l), tz)),
            _ => String::new(),
        };
        let mut lines = vec![format!("### [{proj}{tag}] {title}{span}")];
        if !s.qa.is_empty() {
            if s.qa_dropped > 0 {
                lines.push(format!("- (앞부분 질답 {}개 생략)", s.qa_dropped));
            }
            for turn in &s.qa {
                let q = squash_ws(&turn.question);
                let a = squash_ws(&turn.answer);
                let mut seg = if turn.time.is_empty() {
                    format!("- Q: {q}")
                } else {
                    format!("- {} Q: {q}", turn.time)
                };
                if !a.is_empty() {
                    seg.push_str(&format!(" → A: {a}"));
                }
                lines.push(seg);
            }
        } else if let Some(intent) = s.intent.as_deref().filter(|i| !i.is_empty()) {
            lines.push(format!("- 요청: {}", squash_ws(intent)));
        }
        if !s.files_edited.is_empty() {
            let shown: Vec<&str> = s
                .files_edited
                .iter()
                .take(max_files)
                .map(|p| base_name(p))
                .collect();
            let more = if s.files_edited.len() > shown.len() {
                format!(" 외 {}개", s.files_edited.len() - shown.len())
            } else {
                String::new()
            };
            lines.push(format!("- 수정: {}{more}", shown.join(", ")));
        }
        blocks.push((format!("[{proj}{tag}] {title}"), lines.join("\n")));
    }
    blocks
}

/// `render_session_blocks` 결과를 요약 신호에 붙일 '세션 질답 흐름' 섹션 문자열로.
pub fn render_session_section(blocks: &[(String, String)]) -> String {
    let body = blocks
        .iter()
        .map(|(_, b)| b.as_str())
        .filter(|b| !b.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if body.is_empty() {
        return String::new();
    }
    format!("{SESSION_SECTION_HEADER}\n{body}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::analyze;
    use crate::model::{CalendarData, CalendarEvent, GitCommit, GitData, QaTurn, SessionData};
    use crate::time::get_tz;
    use crate::time::parse_iso;
    use chrono::NaiveDate;

    fn sample() -> DailyData {
        let mut d = DailyData::new(NaiveDate::from_ymd_opt(2026, 7, 6).unwrap(), "Asia/Seoul");
        d.calendar = Some(CalendarData {
            events: vec![CalendarEvent {
                title: Some("스프린트 회의".into()),
                start: Some("2026-07-06T10:00:00+09:00".into()),
                end: Some("2026-07-06T11:00:00+09:00".into()),
                all_day: false,
                location: Some("회의실 A".into()),
                attendees: vec!["김".into(), "이".into()],
                ..Default::default()
            }],
        });
        d.git = Some(GitData {
            commits: vec![GitCommit {
                repo: "repo".into(),
                hash: "abcdef1234".into(),
                author: "me".into(),
                when: parse_iso("2026-07-06T01:00:00Z").unwrap(),
                subject: "fix: login bug".into(),
                files_changed: 2,
                insertions: 10,
                deletions: 3,
                repo_path: String::new(),
            }],
        });
        let mut tools = indexmap::IndexMap::new();
        tools.insert("Edit".to_string(), 1);
        tools.insert("Bash".to_string(), 1);
        d.claude = Some(SessionData {
            sessions: vec![Session {
                session_id: Some("s1".into()),
                project: Some("repo".into()),
                cwd: Some("D:\\repo".into()),
                git_branch: Some("main".into()),
                title: Some("로그인 버그 수정".into()),
                intent: Some("로그인 고쳐줘".into()),
                files_edited: vec!["D:\\repo\\auth.py".into()],
                commands: vec!["pytest -q".into()],
                tool_counts: tools,
                output_tokens: 500,
                ..Default::default()
            }],
        });
        d
    }

    #[test]
    fn facts_has_sections_and_exact_lines() {
        let md = render_facts(&sample(), get_tz("Asia/Seoul"));
        assert!(md.starts_with(
            "# 2026-07-06 업무 데이터 (Asia/Seoul)\n\n## 📅 캘린더 일정 (NaverWorks)\n"
        ));
        assert!(md.contains("- **10:00–11:00** 스프린트 회의 (@회의실 A, 참석 2명)"));
        assert!(md.contains("## 💾 Git 커밋\n- 총 **1커밋** · 1개 저장소 · +10/-3 줄\n- **repo**\n    - `abcdef12` fix: login bug (+10/-3, 2파일)"));
        assert!(md.contains("## 🤖 Claude Code 작업\n- 총 **1세션** · 출력 500 토큰\n- **repo** [main]: 로그인 버그 수정\n    - 요청: 로그인 고쳐줘\n    - 도구: Edit 1, Bash 1\n    - 수정 파일(1): auth.py\n    - `$ pytest -q`"));
        assert!(!md.contains("Codex")); // Codex 없으면 섹션 생략
        assert!(md.ends_with("\n") && !md.ends_with("\n\n"));
    }

    #[test]
    fn facts_empty_and_meta_excluded() {
        let mut d = DailyData::new(NaiveDate::from_ymd_opt(2026, 7, 6).unwrap(), "Asia/Seoul");
        let md = render_facts(&d, get_tz("Asia/Seoul"));
        assert!(md.contains("- (일정 없음 또는 미연동)"));
        assert!(md.contains("- (커밋 없음)"));
        assert!(md.contains("- (세션 없음)"));

        d.claude = Some(SessionData {
            sessions: vec![
                Session {
                    project: Some("proj".into()),
                    title: Some("기능 작업".into()),
                    intent: Some("기능 추가".into()),
                    output_tokens: 100,
                    ..Default::default()
                },
                Session {
                    project: Some("proj".into()),
                    intent: Some(format!("{WORKLOG_SENTINEL} 요약 실행")),
                    output_tokens: 5000,
                    ..Default::default()
                },
            ],
        });
        d.warnings.push("[git] 경고".into());
        let md = render_facts(&d, get_tz("Asia/Seoul"));
        assert!(md.contains("1세션"));
        assert!(!md.contains("2세션"));
        assert!(!md.contains("5,000"));
        assert!(md.contains("## ⚠️ 수집 경고\n- [git] 경고"));
    }

    #[test]
    fn work_signal_is_clean() {
        let mut d = DailyData::new(NaiveDate::from_ymd_opt(2026, 7, 6).unwrap(), "Asia/Seoul");
        d.git = Some(GitData {
            commits: vec![GitCommit {
                repo: "repo".into(),
                hash: "a".repeat(10),
                author: "me".into(),
                when: parse_iso("2026-07-06T01:00:00Z").unwrap(),
                subject: "feat: X 추가".into(),
                files_changed: 1,
                insertions: 1,
                deletions: 0,
                repo_path: String::new(),
            }],
        });
        let sess =
            |id: &str, proj: &str, title: Option<&str>, intent: &str, files: usize| Session {
                session_id: Some(id.into()),
                project: Some(proj.into()),
                title: title.map(str::to_string),
                intent: Some(intent.into()),
                files_edited: (0..files).map(|i| format!("f{i}.py")).collect(),
                commands: vec!["git commit -m x".into(), "pytest".into()],
                ..Default::default()
            };
        d.claude = Some(SessionData {
            sessions: vec![
                sess("1", "repo", Some("로그인 버그 수정"), "로그인 고쳐줘", 2),
                sess("2", "repo", Some("로그인 버그 수정"), "또", 1),
                sess(
                    "3",
                    "Daily Work Log",
                    Some("뭔가"),
                    "너는 개발자의 하루 활동 로그를 바탕으로 업무일지를 작성",
                    0,
                ),
            ],
        });
        let sig = render_work_signal(&d, get_tz("Asia/Seoul"), "가용 데이터 X");
        assert!(sig.starts_with(
            "가용 데이터 X\n\n## Git 커밋 (완료된 결과물 · 1차 근거)\n- **repo**: feat: X 추가\n\n"
        ));
        assert_eq!(sig.matches("로그인 버그 수정").count(), 1);
        assert!(sig.contains("- **repo**: 로그인 버그 수정(2파일)"));
        assert!(!sig.contains("pytest") && !sig.contains("git commit"));
        assert!(!sig.contains("로그인 고쳐줘"));
        assert!(!sig.contains("개발자의 하루 활동 로그"));
        assert!(!sig.contains("Daily Work Log"));
        assert!(
            render_work_signal(
                &DailyData::new(d.target_date, "Asia/Seoul"),
                get_tz("Asia/Seoul"),
                ""
            )
            .is_empty()
        );
    }

    #[test]
    fn meta_session_detection_all_versions() {
        let sess = |intent: &str, title: &str| Session {
            project: Some("Daily Work Log".into()),
            title: Some(title.into()),
            intent: Some(intent.into()),
            ..Default::default()
        };
        assert!(is_meta_session(&sess(
            "너는 개발자의 하루 활동 로그(...)를 바탕으로 ...",
            ""
        )));
        assert!(is_meta_session(&sess(
            "너는 하루치 개발 활동 데이터를 '업무일지'로 압축하는 도구다.",
            ""
        )));
        assert!(is_meta_session(&sess(
            "너는 하루치 개발 활동 데이터를 '업무일지'로 문서화하는 도구다.",
            ""
        )));
        assert!(is_meta_session(&sess(
            &format!("{WORKLOG_SENTINEL}\n무엇이든"),
            ""
        )));
        assert!(is_meta_session(&sess("  너는 하루치 개발 활동 로그를", "")));
        assert!(!is_meta_session(&sess(
            "업무일지 생성기를 만들려고 해. activity watch 감시...",
            ""
        )));
        assert!(!is_meta_session(&sess(
            "로그인 버그 고쳐줘",
            "로그인 버그 수정"
        )));
        assert!(!is_meta_session(&sess(
            "render 함수에서 정제된 요약 신호 렌더링 고쳐줘",
            ""
        )));
        assert!(!is_meta_session(&sess(
            "업무일지 본문을 작성하는 로직 수정",
            "업무일지 렌더 수정"
        )));
        assert!(!is_meta_session(&sess(
            "요약 프롬프트 고쳐줘 — 하루치 개발 활동 로그 문구 포함",
            ""
        )));
    }

    #[test]
    fn session_blocks_and_section() {
        let tz = get_tz("Asia/Seoul");
        let s = Session {
            session_id: Some("s1".into()),
            project: Some("proj".into()),
            title: Some("세션 제목".into()),
            intent: Some("첫 요청".into()),
            qa: vec![
                QaTurn {
                    time: "10:00".into(),
                    question: "A 주제 질문".into(),
                    answer: "A 답".into(),
                },
                QaTurn {
                    time: "11:00".into(),
                    question: "B 주제\n질문".into(),
                    answer: String::new(),
                },
            ],
            qa_dropped: 2,
            files_edited: (0..10).map(|i| format!("D:/p/f{i}.py")).collect(),
            first_ts: parse_iso("2026-07-08T01:00:00Z"),
            last_ts: parse_iso("2026-07-08T02:00:00Z"),
            ..Default::default()
        };
        let mut d = DailyData::new(NaiveDate::from_ymd_opt(2026, 7, 8).unwrap(), "Asia/Seoul");
        d.claude = Some(SessionData { sessions: vec![s] });
        let blocks = render_session_blocks(&d, tz, 8);
        assert_eq!(blocks.len(), 1);
        let (label, block) = &blocks[0];
        assert_eq!(label, "[proj] 세션 제목");
        assert_eq!(
            block,
            "### [proj] 세션 제목 10:00–11:00\n- (앞부분 질답 2개 생략)\n- 10:00 Q: A 주제 질문 → A: A 답\n- 11:00 Q: B 주제 질문\n- 수정: f0.py, f1.py, f2.py, f3.py, f4.py, f5.py, f6.py, f7.py 외 2개"
        );
        let sec = render_session_section(&blocks);
        assert!(sec.starts_with(SESSION_SECTION_HEADER));
        assert!(sec.ends_with("외 2개\n"));
        assert!(render_session_section(&[]).is_empty());
    }

    #[test]
    fn delimiter_in_title_does_not_fracture_block_and_codex_tag() {
        let tz = get_tz("Asia/Seoul");
        let s = Session {
            project: Some("proj".into()),
            intent: Some("리뷰\n\n### 대상 정리".into()),
            agent: crate::model::Agent::Codex,
            qa: vec![QaTurn {
                time: "10:00".into(),
                question: "q".into(),
                answer: "a".into(),
            }],
            ..Default::default()
        };
        let mut d = DailyData::new(NaiveDate::from_ymd_opt(2026, 7, 8).unwrap(), "Asia/Seoul");
        d.codex = Some(SessionData { sessions: vec![s] });
        let blocks = render_session_blocks(&d, tz, 8);
        let (_, block) = &blocks[0];
        assert!(!block.contains("\n\n### "));
        assert!(block.starts_with("### [proj · Codex] 리뷰 ### 대상 정리\n"));
        let body = render_session_section(&blocks);
        let body = body.split_once(SESSION_SECTION_HEADER).unwrap().1.trim();
        assert_eq!(body.split("\n\n### ").count(), 1);

        // 질답 없고 intent 만 있으면 '요청' 줄
        let s2 = Session {
            project: Some("p".into()),
            title: Some("t".into()),
            intent: Some("해줘  줄바꿈\n포함".into()),
            ..Default::default()
        };
        let mut d2 = DailyData::new(d.target_date, "Asia/Seoul");
        d2.claude = Some(SessionData { sessions: vec![s2] });
        assert_eq!(
            render_session_blocks(&d2, tz, 8)[0].1,
            "### [p] t\n- 요청: 해줘 줄바꿈 포함"
        );
    }

    #[test]
    fn analysis_markdown_and_timeline_text() {
        let a = analyze(&crate::analyze::tests::sample(), get_tz("Asia/Seoul"));
        let md = render_analysis(&a);
        assert!(md.contains("## ⭐ 핵심 성과\n- [기능] feat: 큰 기능 (repoA)"));
        assert!(md.contains("## 📊 오늘 지표\n- 커밋 **2** (+403/−3) · 저장소 1 · AI **1세션** · 출력 5K토큰 · 활동 09:00–12:00"));
        assert!(md.contains("- 커밋 타입: 기능 1 · 버그 1"));
        assert!(md.contains("- 작업 성격: **구현형** (Edit 10 · Read 2)"));
        assert!(md.contains("### 프로젝트별 집중\n| 프로젝트 | 집중시간 | 세션 | 파일 | 커밋 | 변경 |\n|---|--:|--:|--:|--:|--:|\n| repoA | 1h 30m | 1 | 2 | 2 | +403/−3 |"));
        assert!(md.contains("## 🕐 타임라인\n- `09:00–10:30` 🤖 기능 구현 · repoA\n- `10:00` 💾 [기능] feat: 큰 기능 · repoA\n- `12:00` 💾 [버그] fix: 작은 버그 · repoA"));

        let mut d = crate::analyze::tests::sample();
        d.calendar = Some(CalendarData {
            events: vec![CalendarEvent {
                title: Some("회의A".into()),
                start: Some("2026-07-06T01:30:00+00:00".into()),
                end: Some("2026-07-06T02:00:00+00:00".into()),
                ..Default::default()
            }],
        });
        let txt = render_timeline_for_llm(&analyze(&d, get_tz("Asia/Seoul")));
        assert!(txt.starts_with("## 시간순 이벤트 (이 순서로 '시간대별 업무'를 서술)\n"));
        assert!(txt.contains("- 10:30–11:00 [회의] 회의A\n"));
        assert!(txt.contains("[커밋]") && txt.contains("[작업]"));
        assert_eq!(render_timeline_for_llm(&Analysis::default()), "");
    }

    #[test]
    fn notes_appear_in_facts_signal_and_timeline() {
        let tz = get_tz("Asia/Seoul");
        let mut d = DailyData::new(NaiveDate::from_ymd_opt(2026, 9, 4).unwrap(), "Asia/Seoul");
        d.notes.push(crate::model::NoteItem {
            id: 7,
            ts: parse_iso("2026-09-04T01:35:00Z").unwrap(),
            text: "김팀장 구두 요청 — 결제 API 타임아웃\n3초→10초".into(),
            tags: vec!["요청".into()],
            mentions: vec!["김팀장".into()],
            source: "app".into(),
        });
        let facts = render_facts(&d, tz);
        assert!(facts.contains("## 📝 메모\n- **10:35** 김팀장 구두 요청 — 결제 API 타임아웃\n3초→10초 [#요청 @김팀장]\n"));
        let sig = render_work_signal(&d, tz, "");
        assert!(sig.starts_with("## 메모 (사용자가 직접 남긴 1차 사실 · 구두 요청·결정·할 일)\n- 10:35 김팀장 구두 요청 — 결제 API 타임아웃 3초→10초 [#요청 @김팀장]\n"));
        let a = analyze(&d, tz);
        assert_eq!(a.kpis.notes, 1);
        assert_eq!(a.timeline.len(), 1);
        assert_eq!(a.timeline[0].kind, "note");
        assert_eq!(a.timeline[0].start, "10:35");
        let md = render_analysis(&a);
        assert!(md.contains("· 메모 1건"));
        assert!(md.contains("## 🕐 타임라인\n- `10:35` 📝 김팀장 구두 요청 — 결제 API 타임아웃\n3초→10초 [#요청 @김팀장]"));
        let txt = render_timeline_for_llm(&a);
        assert!(txt.contains("- 10:35 [메모] 김팀장 구두 요청"));
        assert!(!txt.contains("· 메모\n"));
        // 메모가 없으면 어떤 섹션도 생기지 않는다(v1 출력과 동일)
        let empty = DailyData::new(d.target_date, "Asia/Seoul");
        assert!(!render_facts(&empty, tz).contains("메모"));
        assert!(!render_analysis(&analyze(&empty, tz)).contains("메모"));
    }

    #[test]
    fn helpers() {
        assert_eq!(fmt_thousands(0), "0");
        assert_eq!(fmt_thousands(999), "999");
        assert_eq!(fmt_thousands(1000), "1,000");
        assert_eq!(fmt_thousands(1234567), "1,234,567");
        assert_eq!(tok(999), "999토큰");
        assert_eq!(tok(303_000), "303K토큰");
        assert_eq!(tok(1_250_000), "1.2M토큰");
        assert_eq!(base_name("D:\\a\\b.py"), "b.py");
        assert_eq!(base_name("/x/y"), "y");
        assert_eq!(base_name("plain"), "plain");
    }
}
