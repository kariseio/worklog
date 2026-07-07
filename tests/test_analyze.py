"""분석 계층(analyze.py) 단위 테스트."""

from __future__ import annotations

from datetime import date, datetime, timezone

from worklog.analyze import analyze, classify_commit
from worklog.models import (
    ClaudeData,
    ClaudeSession,
    DailyData,
    GitCommit,
    GitData,
)
from worklog.render import render_analysis
from worklog.util import get_tz


def test_classify_commit():
    assert classify_commit("feat: X")[0] == "feat"
    assert classify_commit("fix(auth): Y")[0] == "fix"
    assert classify_commit("refactor: Z")[0] == "refactor"
    assert classify_commit("Feat: stage-suda EBS 확장")[0] == "infra"   # EBS 힌트 우선
    assert classify_commit("로그인 버그 수정")[0] == "fix"              # 키워드
    assert classify_commit("검색 노드 추가")[0] == "feat"              # 키워드
    assert classify_commit("random text")[0] == "other"


def _sample() -> DailyData:
    d = DailyData(target_date=date(2026, 7, 6), tz_name="Asia/Seoul")
    d.git = GitData(commits=[
        GitCommit(repo="repoA", hash="a" * 10, author="me",
                  when=datetime(2026, 7, 6, 1, 0, tzinfo=timezone.utc),
                  subject="feat: 큰 기능", files_changed=5, insertions=400, deletions=2),
        GitCommit(repo="repoA", hash="b" * 10, author="me",
                  when=datetime(2026, 7, 6, 3, 0, tzinfo=timezone.utc),
                  subject="fix: 작은 버그", files_changed=1, insertions=3, deletions=1),
    ])
    d.claude = ClaudeData(sessions=[
        ClaudeSession(session_id="1", project="repoA", cwd=r"D:\repoA", git_branch="main",
                      title="기능 구현", intent="x", files_edited=["a.py", "b.py"],
                      commands=[], tool_counts={"Edit": 10, "Read": 2}, output_tokens=5000,
                      first_ts=datetime(2026, 7, 6, 0, 0, tzinfo=timezone.utc),
                      last_ts=datetime(2026, 7, 6, 1, 30, tzinfo=timezone.utc)),
    ])
    return d


def test_analyze_kpis_and_rollup():
    a = analyze(_sample(), get_tz("Asia/Seoul"))
    assert a.kpis.commits == 2
    assert a.kpis.insertions == 403
    assert a.kpis.repos == 1
    assert a.kpis.sessions == 1
    assert a.kpis.tokens == 5000
    assert a.kpis.files_edited == 2
    assert a.kpis.span_start == "09:00" and a.kpis.span_end == "12:00"

    p = a.projects[0]
    assert p.project == "repoA"
    assert p.minutes == 90     # 00:00→01:30 UTC = 90분
    assert p.commits == 2 and p.files == 2 and p.insertions == 403


def test_analyze_types_style_highlights_timeline():
    a = analyze(_sample(), get_tz("Asia/Seoul"))
    assert a.commit_types.get("기능") == 1
    assert a.commit_types.get("버그") == 1
    assert "구현형" in a.work_style                 # Edit 우세
    assert "큰 기능" in a.highlights[0]             # feat + 큰 변경량이 최상위

    kinds = [e.kind for e in a.timeline]
    assert kinds.count("session") == 1 and kinds.count("commit") == 2
    starts = [e.start for e in a.timeline]
    assert starts == sorted(starts)                # 시간순 정렬


def test_render_analysis_markdown():
    md = render_analysis(analyze(_sample(), get_tz("Asia/Seoul")))
    assert "핵심 성과" in md
    assert "오늘 지표" in md
    assert "프로젝트별 집중" in md
    assert "타임라인" in md
    assert "repoA" in md


def test_analyze_weaves_calendar_meetings_into_timeline():
    from worklog.models import CalendarData, CalendarEvent

    d = _sample()
    d.calendar = CalendarData(events=[CalendarEvent(
        title="스프린트 회의", start="2026-07-06T01:30:00+00:00",
        end="2026-07-06T02:00:00+00:00", all_day=False, location="회의실A")])
    a = analyze(d, get_tz("Asia/Seoul"))

    assert a.kpis.meetings == 1
    mtgs = [e for e in a.timeline if e.kind == "meeting"]
    assert len(mtgs) == 1
    assert "스프린트 회의" in mtgs[0].label
    assert mtgs[0].start == "10:30"                 # 01:30 UTC → 10:30 KST
    starts = [e.start for e in a.timeline]
    assert starts == sorted(starts)                 # 회의가 시간순으로 끼워짐


def test_render_timeline_for_llm():
    from worklog.models import CalendarData, CalendarEvent
    from worklog.render import render_timeline_for_llm

    d = _sample()
    d.calendar = CalendarData(events=[CalendarEvent(
        title="회의A", start="2026-07-06T01:30:00+00:00",
        end="2026-07-06T02:00:00+00:00", all_day=False)])
    txt = render_timeline_for_llm(analyze(d, get_tz("Asia/Seoul")))
    assert "시간순 이벤트" in txt
    assert "[회의] 회의A" in txt
    assert "[커밋]" in txt and "[작업]" in txt
