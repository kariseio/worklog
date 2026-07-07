"""결정론적 분석 계층.

이미 수집되지만 표현에 안 쓰이던 데이터(세션 시간 first_ts/last_ts, tool_counts,
output_tokens, 커밋 변경량)를 뽑아 지표·핵심성과·프로젝트 집중시간·커밋타입·타임라인을 만든다.
LLM 을 쓰지 않으므로 항상 사실에 정확히 일치한다.

주의: '변경량 = 노력' 프록시는 리팩터/생성 코드에 편향된다. 참고 지표로만 볼 것.
"""

from __future__ import annotations

import re
from dataclasses import asdict, dataclass, field
from datetime import datetime

from .models import DailyData
from .render import _is_meta_session
from .util import parse_iso

# --------------------------------------------------------------------------- #
# 커밋 타입 분류 (conventional commit 우선, 없으면 키워드 휴리스틱)
# --------------------------------------------------------------------------- #

_CONV = re.compile(r"^\s*(\w+)(?:\([^)]*\))?!?:", re.IGNORECASE)
_TYPE_MAP = {
    "feat": ("feat", "기능"), "feature": ("feat", "기능"),
    "fix": ("fix", "버그"), "bugfix": ("fix", "버그"), "hotfix": ("fix", "버그"),
    "refactor": ("refactor", "리팩터"),
    "perf": ("perf", "성능"),
    "docs": ("docs", "문서"), "doc": ("docs", "문서"),
    "test": ("test", "테스트"), "tests": ("test", "테스트"),
    "chore": ("chore", "기타"), "build": ("chore", "빌드"), "ci": ("chore", "CI"),
    "style": ("style", "스타일"),
}
_INFRA_HINTS = ("terraform", "ebs", "helm", "k8s", "kubernetes", "deploy", "infra", "docker")
_KEYWORD_HINTS = [
    (("추가", "신설", "add", "새로", "생성", "구축", "도입"), ("feat", "기능")),
    (("수정", "버그", "fix", "오류", "문제", "해결", "고침"), ("fix", "버그")),
    (("리팩", "refactor", "정리", "개선"), ("refactor", "리팩터")),
    (("문서", "docs", "readme"), ("docs", "문서")),
    (("테스트", "test"), ("test", "테스트")),
    (("롤백", "revert", "되돌"), ("revert", "롤백")),
]


def classify_commit(subject: str) -> tuple[str, str]:
    """커밋 subject → (type_key, 한글라벨). 예: 'feat: X' → ('feat','기능')."""
    s = subject or ""
    low = s.lower()
    if any(h in low for h in _INFRA_HINTS):
        return ("infra", "인프라")
    m = _CONV.match(s)
    if m:
        key = m.group(1).lower()
        if key in _TYPE_MAP:
            return _TYPE_MAP[key]
    for words, res in _KEYWORD_HINTS:
        if any(w in low for w in words):
            return res
    return ("other", "기타")


_TYPE_WEIGHT = {"feat": 3, "infra": 2, "fix": 2, "refactor": 2, "perf": 2, "docs": 1}


# --------------------------------------------------------------------------- #
# 데이터 구조
# --------------------------------------------------------------------------- #


@dataclass
class Kpis:
    commits: int = 0
    insertions: int = 0
    deletions: int = 0
    repos: int = 0
    sessions: int = 0
    tokens: int = 0
    files_edited: int = 0
    meetings: int = 0
    span_start: str | None = None
    span_end: str | None = None


@dataclass
class ProjectRollup:
    project: str
    minutes: int = 0
    sessions: int = 0
    files: int = 0
    commits: int = 0
    insertions: int = 0
    deletions: int = 0


@dataclass
class TimelineEvent:
    kind: str            # "session" | "commit"
    start: str           # HH:MM
    end: str | None      # HH:MM (session)
    project: str
    label: str
    ctype: str | None = None    # 커밋 타입 라벨


@dataclass
class Analysis:
    kpis: Kpis = field(default_factory=Kpis)
    projects: list[ProjectRollup] = field(default_factory=list)
    commit_types: dict = field(default_factory=dict)   # 라벨 -> 건수
    tool_profile: dict = field(default_factory=dict)   # 도구 -> 호출수
    work_style: str = ""
    highlights: list = field(default_factory=list)
    timeline: list = field(default_factory=list)

    def to_dict(self) -> dict:
        return asdict(self)


# --------------------------------------------------------------------------- #
# 분석
# --------------------------------------------------------------------------- #


def _hm(dt: datetime | None, tz) -> str | None:
    if dt is None:
        return None
    return dt.astimezone(tz).strftime("%H:%M")


def analyze(data: DailyData, tz) -> Analysis:
    a = Analysis()

    commits = data.git.commits if data.git else []
    # 업무일지 생성기 자신의 요약 세션(피드백 루프)은 지표/타임라인에서 제외
    sessions = [s for s in (data.claude.sessions if data.claude else []) if not _is_meta_session(s)]

    # --- KPI ---
    a.kpis.commits = len(commits)
    a.kpis.insertions = sum(c.insertions for c in commits)
    a.kpis.deletions = sum(c.deletions for c in commits)
    a.kpis.repos = len(data.git.repos) if data.git else 0
    a.kpis.sessions = len(sessions)
    # meta(자동요약) 세션을 뺀 sessions 에서 직접 합산(total_output_tokens 는 meta 포함이라 부풀려짐)
    a.kpis.tokens = sum(s.output_tokens for s in sessions)
    a.kpis.files_edited = sum(len(s.files_edited) for s in sessions)

    times = [c.when for c in commits if c.when]
    times += [s.first_ts for s in sessions if s.first_ts]
    times += [s.last_ts for s in sessions if s.last_ts]
    if times:
        a.kpis.span_start = _hm(min(times), tz)
        a.kpis.span_end = _hm(max(times), tz)

    # --- 커밋 타입 분포 ---
    for c in commits:
        _, label = classify_commit(c.subject)
        a.commit_types[label] = a.commit_types.get(label, 0) + 1

    # --- 도구 프로파일 + 작업 성격 ---
    for s in sessions:
        for tool, n in s.tool_counts.items():
            a.tool_profile[tool] = a.tool_profile.get(tool, 0) + n
    a.work_style = _work_style(a.tool_profile)

    # --- 프로젝트별 롤업 (Claude project ↔ git repo 이름으로 조인) ---
    roll: dict[str, ProjectRollup] = {}

    def bucket(name: str) -> ProjectRollup:
        return roll.setdefault(name, ProjectRollup(project=name))

    for s in sessions:
        proj = s.project or "?"
        r = bucket(proj)
        r.sessions += 1
        r.files += len(s.files_edited)
        if s.first_ts and s.last_ts:
            mins = int((s.last_ts - s.first_ts).total_seconds() // 60)
            r.minutes += max(mins, 0)
    for c in commits:
        r = bucket(c.repo)
        r.commits += 1
        r.insertions += c.insertions
        r.deletions += c.deletions

    a.projects = sorted(
        roll.values(),
        key=lambda p: (p.minutes, p.commits, p.files),
        reverse=True,
    )

    # --- 핵심 성과 (Top 3) ---
    a.highlights = _highlights(commits, sessions)

    # --- 타임라인 (세션 구간 + 커밋 시각 + 캘린더 회의) ---
    events: list[TimelineEvent] = []
    for s in sessions:
        if not s.first_ts:
            continue
        title = (s.title or s.intent or "").strip()
        if not title:
            continue
        events.append(TimelineEvent(
            kind="session", start=_hm(s.first_ts, tz) or "", end=_hm(s.last_ts, tz),
            project=s.project or "?", label=title[:70]))
    for c in commits:
        if not c.when:
            continue
        _, label = classify_commit(c.subject)
        events.append(TimelineEvent(
            kind="commit", start=_hm(c.when, tz) or "", end=None,
            project=c.repo, label=c.subject[:70], ctype=label))

    events += _meeting_events(data, tz, a.kpis)
    events.sort(key=lambda e: e.start)
    a.timeline = events

    return a


def _meeting_events(data: DailyData, tz, kpis: Kpis) -> list["TimelineEvent"]:
    """NaverWorks 캘린더 회의를 타임라인 이벤트로. (회의를 시간축에 끼워넣음)"""
    out: list[TimelineEvent] = []
    if not (data.calendar and data.calendar.events):
        return out
    for e in data.calendar.events:
        if e.all_day:
            out.append(TimelineEvent(
                kind="meeting", start="00:00", end=None, project="회의",
                label=(e.title or "회의") + " (종일)"))
            kpis.meetings += 1
            continue
        st = parse_iso(e.start)
        if not st:
            continue   # 시작 파싱 실패 → 타임라인에 안 넣고 카운트도 안 함
        en = parse_iso(e.end)
        loc = f" @{e.location}" if e.location else ""
        out.append(TimelineEvent(
            kind="meeting", start=_hm(st, tz) or "", end=_hm(en, tz) if en else None,
            project="회의", label=(e.title or "회의") + loc))
        kpis.meetings += 1
    return out


def _work_style(tool_profile: dict) -> str:
    if not tool_profile:
        return ""
    edit = sum(tool_profile.get(t, 0) for t in ("Edit", "Write", "MultiEdit", "NotebookEdit"))
    read = sum(tool_profile.get(t, 0) for t in ("Read", "Grep", "Glob"))
    run = sum(tool_profile.get(t, 0) for t in ("Bash", "PowerShell"))
    ranked = sorted([("구현형", edit), ("탐색형", read), ("실행형", run)], key=lambda x: x[1], reverse=True)
    if ranked[0][1] == 0:
        return ""
    top, second = ranked[0], ranked[1]
    if second[1] and top[1] <= second[1] * 1.3:
        return f"{top[0]}·{second[0]} 혼합"
    return top[0]


def _highlights(commits, sessions) -> list:
    scored = []
    for c in commits:
        key, label = classify_commit(c.subject)
        score = _TYPE_WEIGHT.get(key, 1)
        if c.insertions > 300:
            score += 2
        elif c.insertions > 50:
            score += 1
        scored.append((score, c.insertions, f"[{label}] {c.subject} ({c.repo})"))
    if scored:
        scored.sort(key=lambda x: (x[0], x[1]), reverse=True)
        return [s[2] for s in scored[:3]]
    # 커밋이 없으면 파일 많이 만진 세션 상위
    sess = sorted(sessions, key=lambda s: len(s.files_edited), reverse=True)
    out = []
    for s in sess[:3]:
        title = (s.title or s.intent or "").strip()
        if title:
            n = len(s.files_edited)
            out.append(f"{title} ({s.project or '?'}{', ' + str(n) + '파일' if n else ''})")
    return out
