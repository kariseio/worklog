"""세션 질답 수집 + 세션 블록 렌더 + summarize_day map-reduce 분기."""

from __future__ import annotations

import json
import logging
import os
from datetime import date, datetime

import pytest

try:
    from zoneinfo import ZoneInfo
except ImportError:  # pragma: no cover
    ZoneInfo = None

from worklog import summarize as S
from worklog.collectors.base import CollectContext
from worklog.collectors.claude_logs import ClaudeLogCollector
from worklog.config import ClaudeConfig, SummarizerConfig
from worklog.models import ClaudeData, ClaudeSession, DailyData, QATurn
from worklog.render import render_session_blocks, render_session_section


def _ctx(tz):
    start = datetime(2026, 7, 8, 0, 0, tzinfo=tz)
    end = datetime(2026, 7, 9, 0, 0, tzinfo=tz)
    return CollectContext(target_date=date(2026, 7, 8), start=start, end=end,
                          tz=tz, tz_name="Asia/Seoul", logger=logging.getLogger("t"))


def _write_session(tmp_path, recs, start_ts):
    proj = tmp_path / "projects" / "-home-u-proj"
    proj.mkdir(parents=True)
    f = proj / "sess.jsonl"
    f.write_text("\n".join(json.dumps(r) for r in recs), encoding="utf-8")
    os.utime(str(f), (start_ts + 3600, start_ts + 3600))   # mtime 을 그날 안으로 고정
    return str(tmp_path / "projects")


@pytest.mark.skipif(ZoneInfo is None, reason="zoneinfo 필요")
def test_qa_capture_multiple_topics(tmp_path):
    tz = ZoneInfo("Asia/Seoul")
    recs = [
        {"type": "user", "sessionId": "s1", "cwd": "/home/u/proj",
         "timestamp": "2026-07-08T01:00:00Z", "message": {"content": "첫 요청 내용"}},
        {"type": "assistant", "timestamp": "2026-07-08T01:00:05Z",
         "message": {"content": [{"type": "text", "text": "첫 응답 요지"},
                                 {"type": "tool_use", "name": "Edit", "input": {"file_path": "/home/u/proj/a.py"}}],
                     "usage": {"output_tokens": 10}}},
        {"type": "user", "sessionId": "s1", "cwd": "/home/u/proj",
         "timestamp": "2026-07-08T02:00:00Z", "message": {"content": "둘째 다른 주제"}},
        {"type": "assistant", "timestamp": "2026-07-08T02:00:05Z",
         "message": {"content": [{"type": "text", "text": "둘째 응답"}], "usage": {"output_tokens": 5}}},
        # tool_result 만 있는 user 레코드는 '질문'으로 잡히면 안 됨
        {"type": "user", "timestamp": "2026-07-08T02:00:10Z",
         "message": {"content": [{"type": "tool_result", "content": "출력"}]}},
    ]
    ctx = _ctx(tz)
    projects = _write_session(tmp_path, recs, ctx.start.timestamp())
    res = ClaudeLogCollector(ClaudeConfig(projects_dir=projects)).collect(ctx)
    sessions = res.data.sessions
    assert len(sessions) == 1
    s = sessions[0]
    assert len(s.qa) == 2                       # 첫 주제만이 아니라 둘 다
    assert s.qa[0].question == "첫 요청 내용"
    assert "첫 응답 요지" in s.qa[0].answer
    assert s.qa[0].time == "10:00"              # 01:00Z + 9h(KST)
    assert s.qa[1].question == "둘째 다른 주제"


@pytest.mark.skipif(ZoneInfo is None, reason="zoneinfo 필요")
def test_session_blocks_include_all_topics():
    tz = ZoneInfo("Asia/Seoul")
    s = ClaudeSession(
        session_id="s1", project="proj", cwd="/p", git_branch=None,
        title="세션 제목", intent="첫 요청",
        qa=[QATurn(time="10:00", question="A 주제 질문", answer="A 답"),
            QATurn(time="11:00", question="B 주제 질문", answer="B 답")],
    )
    data = DailyData(target_date=date(2026, 7, 8), tz_name="Asia/Seoul",
                     claude=ClaudeData(sessions=[s]))
    blocks = render_session_blocks(data, tz)
    assert len(blocks) == 1
    _, block = blocks[0]
    assert "A 주제 질문" in block and "B 주제 질문" in block   # 첫 주제만이 아님
    sec = render_session_section(blocks)
    assert sec.startswith("## Claude Code 세션 (질답 흐름)")
    assert sec.rstrip().split("\n", 1)[0] == S.SESSION_SECTION_HEADER


def test_real_user_text_filters_synthetic():
    from worklog.collectors.claude_logs import _real_user_text

    def rec(txt):
        return {"type": "user", "message": {"content": txt}}

    assert _real_user_text(rec("진짜 질문입니다")) == "진짜 질문입니다"
    assert _real_user_text(rec("<local-command-stdout>출력</local-command-stdout>")) is None
    assert _real_user_text(rec("<command-message>foo</command-message>")) is None
    assert _real_user_text(rec("<system-reminder>x</system-reminder>")) is None


@pytest.mark.skipif(ZoneInfo is None, reason="zoneinfo 필요")
def test_qa_cap_keeps_recent_not_head(tmp_path):
    tz = ZoneInfo("Asia/Seoul")
    recs = []
    for i in range(5):
        recs.append({"type": "user", "sessionId": "s1", "cwd": "/home/u/proj",
                     "timestamp": f"2026-07-08T0{i + 1}:00:00Z", "message": {"content": f"주제{i}"}})
        recs.append({"type": "assistant", "timestamp": f"2026-07-08T0{i + 1}:00:05Z",
                     "message": {"content": [{"type": "text", "text": f"답{i}"}], "usage": {"output_tokens": 1}}})
    ctx = _ctx(tz)
    projects = _write_session(tmp_path, recs, ctx.start.timestamp())
    res = ClaudeLogCollector(ClaudeConfig(projects_dir=projects, max_qa_turns=3)).collect(ctx)
    s = res.data.sessions[0]
    assert len(s.qa) == 3 and s.qa_dropped == 2
    assert [t.question for t in s.qa] == ["주제2", "주제3", "주제4"]   # 최근 것 유지(하루 끝 보존)
    assert s.intent == "주제0"                                      # 서두는 intent 로 보존


@pytest.mark.skipif(ZoneInfo is None, reason="zoneinfo 필요")
def test_delimiter_in_title_does_not_fracture_block():
    tz = ZoneInfo("Asia/Seoul")
    # ai-title 없음 → intent 가 제목이 되고, intent 에 map-reduce 구분자 "\n\n### " 포함
    s = ClaudeSession(session_id="s1", project="proj", cwd="/p", git_branch=None,
                      title=None, intent="리뷰\n\n### 대상 정리",
                      qa=[QATurn(time="10:00", question="q", answer="a")])
    data = DailyData(target_date=date(2026, 7, 8), tz_name="Asia/Seoul",
                     claude=ClaudeData(sessions=[s]))
    blocks = render_session_blocks(data, tz)
    _, block = blocks[0]
    assert "\n\n### " not in block                     # 구조선에 구분자 주입 안 됨
    body = render_session_section(blocks).split(S.SESSION_SECTION_HEADER, 1)[1].strip()
    assert len(body.split("\n\n### ")) == 1            # 세션 1개 → 오분할 없음


def test_condense_chunks_for_oversized_session(monkeypatch):
    calls = []
    monkeypatch.setattr(S, "_resolve_provider", lambda p: "claude_cli")
    monkeypatch.setattr(S, "_call", lambda sy, u, c: (calls.append(sy), "요약")[1])
    cfg = SummarizerConfig(provider="claude_cli", map_reduce_chars=1500, map_workers=1)
    line = "- 10:00 Q: 어떤 긴 질문입니다 → A: 어떤 긴 답변입니다\n"
    big = "### [p] s1\n" + line * 200          # ~ 8k자 > map_reduce_chars → _condense_chunks 발동
    signal = "## Git\n- x\n\n" + S.SESSION_SECTION_HEADER + "\n" + big
    out = S.summarize_day(signal, "2026-07-08", cfg)
    assert out == "요약"
    assert calls.count(S.CONDENSE_SYSTEM_KO) >= 2     # 청크별 압축 여러 번
    assert calls[-1] == S.SYSTEM_KO                    # 마지막은 종합


def test_summarize_day_single_call(monkeypatch):
    calls = []
    monkeypatch.setattr(S, "_resolve_provider", lambda p: "claude_cli")
    monkeypatch.setattr(S, "_call", lambda system, user, cfg: (calls.append(system), "요약")[1])
    cfg = SummarizerConfig(provider="claude_cli", map_reduce_chars=1500)
    out = S.summarize_day("## Git\n- 커밋", "2026-07-08", cfg)
    assert out == "요약"
    assert calls == [S.SYSTEM_KO]               # 가벼우면 단일 호출


def test_summarize_day_map_reduce(monkeypatch):
    calls = []
    monkeypatch.setattr(S, "_resolve_provider", lambda p: "claude_cli")
    monkeypatch.setattr(S, "_call", lambda system, user, cfg: (calls.append(system), "요약")[1])
    cfg = SummarizerConfig(provider="claude_cli", map_reduce_chars=1500, map_workers=2)
    line = "- 10:00 Q: 어떤 주제 질문입니다 → A: 어떤 응답 요지입니다\n"
    blocks = []
    for n in ("s1", "s2", "s3"):
        blocks.append(f"### [p] {n}\n" + line * 28)   # 각 ~800자(>small, <chunk)
    signal = "## Git\n- 커밋\n\n" + S.SESSION_SECTION_HEADER + "\n" + "\n\n".join(blocks)
    assert len(signal) > cfg.map_reduce_chars
    out = S.summarize_day(signal, "2026-07-08", cfg)
    assert out == "요약"
    assert S.CONDENSE_SYSTEM_KO in calls          # 세션별 압축(map)
    assert calls[-1] == S.SYSTEM_KO               # 마지막은 종합(reduce)
    assert calls.count(S.CONDENSE_SYSTEM_KO) == 3  # 세션 3개 각 1회


def test_projects_dir_honors_claude_config_dir(monkeypatch, tmp_path):
    import os

    from worklog.collectors.claude_logs import ClaudeLogCollector
    from worklog.config import ClaudeConfig

    monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / "cfg"))
    col = ClaudeLogCollector(ClaudeConfig(projects_dir=""))
    assert col._projects_dir() == str(tmp_path / "cfg" / "projects")   # CLAUDE_CONFIG_DIR 우선

    monkeypatch.delenv("CLAUDE_CONFIG_DIR", raising=False)
    col2 = ClaudeLogCollector(ClaudeConfig(projects_dir=""))
    assert col2._projects_dir() == os.path.join(os.path.expanduser("~"), ".claude", "projects")
