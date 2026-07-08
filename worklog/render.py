"""수집한 DailyData 를 결정론적 Markdown(사실 정리)으로 변환.

이 결과물은 두 곳에 쓰인다.
  1) LLM 요약기의 입력(= '이 사실들로 업무일지를 써줘')
  2) 최종 문서의 하단 부록(펼침 가능한 raw 데이터)
"""

from __future__ import annotations

from .models import DailyData
from .util import fmt_time, human_duration, parse_iso


def render_facts(data: DailyData, tz) -> str:
    lines: list[str] = []
    lines.append(f"# {data.target_date.isoformat()} 업무 데이터 ({data.tz_name})")
    lines.append("")

    _calendar(lines, data, tz)
    _git(lines, data)
    _claude(lines, data)
    _activitywatch(lines, data)

    if data.warnings:
        lines.append("## ⚠️ 수집 경고")
        for w in data.warnings:
            lines.append(f"- {w}")
        lines.append("")

    return "\n".join(lines).rstrip() + "\n"


def _calendar(lines, data, tz):
    cal = data.calendar
    lines.append("## 📅 캘린더 일정 (NaverWorks)")
    if not cal or not cal.events:
        lines.append("- (일정 없음 또는 미연동)")
        lines.append("")
        return
    for ev in cal.events:
        title = ev.title or "(제목 없음)"
        if ev.all_day:
            when = "종일"
        else:
            s = parse_iso(ev.start)
            e = parse_iso(ev.end)
            when = f"{fmt_time(s, tz)}–{fmt_time(e, tz)}"
        extra = []
        if ev.location:
            extra.append(f"@{ev.location}")
        if ev.attendees:
            extra.append(f"참석 {len(ev.attendees)}명")
        suffix = f" ({', '.join(extra)})" if extra else ""
        lines.append(f"- **{when}** {title}{suffix}")
    lines.append("")


def _git(lines, data):
    git = data.git
    lines.append("## 💾 Git 커밋")
    if not git or not git.commits:
        lines.append("- (커밋 없음)")
        lines.append("")
        return
    total_ins = sum(c.insertions for c in git.commits)
    total_del = sum(c.deletions for c in git.commits)
    lines.append(
        f"- 총 **{len(git.commits)}커밋** · {len(git.repos)}개 저장소 · +{total_ins}/-{total_del} 줄"
    )
    for repo in git.repos:
        lines.append(f"- **{repo}**")
        for c in git.commits_for(repo):
            stat = f"(+{c.insertions}/-{c.deletions}, {c.files_changed}파일)"
            lines.append(f"    - `{c.short_hash}` {c.subject} {stat}")
    lines.append("")


def _claude(lines, data):
    cl = data.claude
    lines.append("## 🤖 Claude Code 작업")
    # analyze 와 동일하게 업무일지 생성기 자신의 요약 세션을 제외(세션 수/토큰 일치).
    sessions = [s for s in (cl.sessions if cl else []) if not _is_meta_session(s)]
    if not sessions:
        lines.append("- (세션 없음)")
        lines.append("")
        return
    total_tokens = sum(s.output_tokens for s in sessions)
    lines.append(
        f"- 총 **{len(sessions)}세션** · 출력 {total_tokens:,} 토큰"
    )
    for s in sessions:
        head = s.title or s.intent or "(제목 없음)"
        proj = s.project or (s.cwd or "?")
        branch = f" [{s.git_branch}]" if s.git_branch else ""
        lines.append(f"- **{proj}**{branch}: {head}")
        if s.intent and s.intent != head:
            lines.append(f"    - 요청: {s.intent}")
        if s.tool_counts:
            tools = ", ".join(
                f"{k} {v}" for k, v in sorted(s.tool_counts.items(), key=lambda kv: kv[1], reverse=True)
            )
            lines.append(f"    - 도구: {tools}")
        if s.files_edited:
            shown = s.files_edited[:8]
            more = f" 외 {len(s.files_edited) - len(shown)}개" if len(s.files_edited) > len(shown) else ""
            lines.append(f"    - 수정 파일({len(s.files_edited)}): " + ", ".join(_base(p) for p in shown) + more)
        if s.commands:
            for cmd in s.commands[:5]:
                lines.append(f"    - `$ {cmd}`")
    lines.append("")


def _activitywatch(lines, data):
    aw = data.activitywatch
    lines.append("## ⏱️ 앱 사용 시간 (ActivityWatch)")
    if not aw or not aw.by_app:
        lines.append("- (데이터 없음 또는 미설치)")
        lines.append("")
        return
    lines.append(f"- 총 활성 시간 **{human_duration(aw.total_active_seconds)}**")
    for app in aw.by_app:
        titles = f" — {app.top_titles[0]}" if app.top_titles else ""
        lines.append(f"- {human_duration(app.seconds)}  {app.app}{titles}")
    lines.append("")


def _base(path: str) -> str:
    return path.replace("\\", "/").rsplit("/", 1)[-1]


# --------------------------------------------------------------------------- #
# 결정론적 분석 섹션 (핵심성과 · 지표 · 프로젝트 집중 · 타임라인)
# --------------------------------------------------------------------------- #


def render_analysis(analysis) -> str:
    """analyze.Analysis → 마크다운. LLM 요약 아래에 붙는 사실 지표 섹션."""
    k = analysis.kpis
    lines: list[str] = []

    if analysis.highlights:
        lines.append("## ⭐ 핵심 성과")
        for h in analysis.highlights:
            lines.append(f"- {h}")
        lines.append("")

    lines.append("## 📊 오늘 지표")
    span = f" · 활동 {k.span_start}–{k.span_end}" if k.span_start else ""
    mtg = f" · 회의 {k.meetings}건" if k.meetings else ""
    lines.append(
        f"- 커밋 **{k.commits}** (+{k.insertions:,}/−{k.deletions:,}) · 저장소 {k.repos} · "
        f"Claude **{k.sessions}세션** · 출력 {_tok(k.tokens)}{mtg}{span}"
    )
    if analysis.commit_types:
        dist = " · ".join(f"{lbl} {n}" for lbl, n in sorted(analysis.commit_types.items(), key=lambda x: x[1], reverse=True))
        lines.append(f"- 커밋 타입: {dist}")
    if analysis.work_style:
        prof = " · ".join(f"{t} {n}" for t, n in sorted(analysis.tool_profile.items(), key=lambda x: x[1], reverse=True)[:5])
        lines.append(f"- 작업 성격: **{analysis.work_style}** ({prof})")
    lines.append("")

    if analysis.projects:
        lines.append("### 프로젝트별 집중")
        lines.append("| 프로젝트 | 집중시간 | 세션 | 파일 | 커밋 | 변경 |")
        lines.append("|---|--:|--:|--:|--:|--:|")
        for p in analysis.projects:
            dur = human_duration(p.minutes * 60) if p.minutes else "–"
            chg = f"+{p.insertions:,}/−{p.deletions:,}" if p.commits else "–"
            lines.append(f"| {p.project} | {dur} | {p.sessions} | {p.files} | {p.commits} | {chg} |")
        lines.append("")
        lines.append("> 집중시간은 세션 첫~마지막 활동 구간 합산 추정치이고, 변경량(±라인)은 노력과 비례하지 않을 수 있습니다.")
        lines.append("")

    if analysis.timeline:
        lines.append("## 🕐 타임라인")
        for e in analysis.timeline:
            when = f"{e.start}–{e.end}" if e.end else e.start
            if e.kind == "commit":
                tag = f"[{e.ctype}] " if e.ctype else ""
                lines.append(f"- `{when}` 💾 {tag}{e.label} · {e.project}")
            elif e.kind == "meeting":
                lines.append(f"- `{when}` 📅 {e.label}")
            else:
                lines.append(f"- `{when}` 🤖 {e.label} · {e.project}")
        lines.append("")

    return "\n".join(lines).rstrip() + "\n"


def _tok(n: int) -> str:
    if n >= 1_000_000:
        return f"{n / 1_000_000:.1f}M토큰"
    if n >= 1_000:
        return f"{n / 1_000:.0f}K토큰"
    return f"{n}토큰"


def render_timeline_for_llm(analysis) -> str:
    """요약기에 넘길 '시간순 이벤트' 텍스트. LLM 이 시간대별 업무 서술에 쓴다."""
    if not analysis.timeline:
        return ""
    kind_label = {"session": "작업", "commit": "커밋", "meeting": "회의"}
    lines = ["## 시간순 이벤트 (이 순서로 '시간대별 업무'를 서술)"]
    for e in analysis.timeline:
        when = f"{e.start}–{e.end}" if e.end else e.start
        lbl = kind_label.get(e.kind, e.kind)
        proj = "" if e.kind == "meeting" else f" · {e.project}"
        lines.append(f"- {when} [{lbl}] {e.label}{proj}")
    return "\n".join(lines) + "\n"


# --------------------------------------------------------------------------- #
# LLM 요약용 '정제 신호' — 소음(원본 프롬프트/명령어/중복)을 제거하고
# '무슨 일을 했는가'의 핵심 근거만 남긴다. (render_facts 는 사람이 볼 전체 부록)
# --------------------------------------------------------------------------- #

# 요약기가 claude CLI 로 만든 세션에 심는 '안정적 표식'.
# 프롬프트 문구가 바뀌어도 이 표식은 불변이라, 요약 세션을 확실히 걸러낼 수 있다.
WORKLOG_SENTINEL = "__WORKLOG_GENERATOR_AUTOSUMMARY__"

# 표식이 없던 과거 요약 세션도 걸러내기 위한 프롬프트 시그니처(버전별).
_META_SIGS = (
    "개발자의 하루 활동 로그",          # v1
    "하루치 개발 활동 데이터를",        # v2·v3 (system 프롬프트 시작부)
    "업무일지'로 압축하는 도구",        # v2
    "업무일지'로 문서화하는 도구",      # v3
    "업무일지를 작성",
    "업무일지 본문을 작성",
    "업무일지 본문만 출력",
    "정제된 요약 신호",
)


def _is_meta_session(s) -> bool:
    text = (s.intent or "") + " " + (s.title or "")
    if WORKLOG_SENTINEL in text:
        return True
    return any(sig in text for sig in _META_SIGS)


def render_work_signal(data: DailyData, tz, header: str = "") -> str:
    """요약기에 넣을 정제된 신호. 커밋 제목 + 세션 제목 중심, 명령어/원문 제외."""
    lines: list[str] = []
    if header:
        lines += [header, ""]

    if data.git and data.git.commits:
        lines.append("## Git 커밋 (완료된 결과물 · 1차 근거)")
        for repo in data.git.repos:
            subs = [c.subject for c in data.git.commits_for(repo)]
            lines.append(f"- **{repo}**: " + "; ".join(subs))
        lines.append("")

    if data.claude and data.claude.sessions:
        lines.append("## Claude Code 작업 (세션 제목 기준)")
        seen: set = set()
        by_proj: dict[str, list] = {}
        for s in data.claude.sessions:
            if _is_meta_session(s):
                continue
            title = (s.title or "").strip() or (s.intent or "").strip()[:60]
            if not title:
                continue
            proj = s.project or "?"
            key = (proj, title)
            if key in seen:
                continue
            seen.add(key)
            by_proj.setdefault(proj, []).append((title, len(s.files_edited)))
        for proj, items in by_proj.items():
            joined = "; ".join(
                t + (f"({n}파일)" if n else "") for t, n in items
            )
            lines.append(f"- **{proj}**: {joined}")
        lines.append("")

    if data.calendar and data.calendar.events:
        lines.append("## 일정")
        for e in data.calendar.events:
            when = "종일" if e.all_day else f"{fmt_time(parse_iso(e.start), tz)}-{fmt_time(parse_iso(e.end), tz)}"
            loc = f" @{e.location}" if e.location else ""
            lines.append(f"- {when} {e.title or ''}{loc}")
        lines.append("")

    if data.activitywatch and data.activitywatch.by_app:
        top = data.activitywatch.by_app[:6]
        lines.append("## 앱 사용 시간 (상위)")
        lines.append("- " + ", ".join(f"{a.app} {human_duration(a.seconds)}" for a in top))
        lines.append("")

    body = "\n".join(lines).strip()
    return (body + "\n") if body else ""
