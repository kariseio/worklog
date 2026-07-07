"""Claude Code 로그 파서 단위 테스트 (외부 의존 없음)."""

from __future__ import annotations

import json
import logging
import os
from datetime import date

from worklog.collectors.base import CollectContext
from worklog.collectors.claude_logs import ClaudeLogCollector
from worklog.config import ClaudeConfig
from worklog.util import get_tz, resolve_day


def _ctx(target: date, tz_name="Asia/Seoul") -> CollectContext:
    tz = get_tz(tz_name)
    t, start, end = resolve_day(target.isoformat(), tz)
    return CollectContext(
        target_date=t, start=start, end=end, tz=tz, tz_name=tz_name,
        logger=logging.getLogger("test"),
    )


def _write_session(dir_path, records):
    os.makedirs(dir_path, exist_ok=True)
    path = os.path.join(dir_path, "sess-1.jsonl")
    with open(path, "w", encoding="utf-8") as f:
        for r in records:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    return path


def test_extracts_intent_files_and_tools(tmp_path):
    projects = tmp_path / "projects"
    sess_dir = projects / "D--demo-repo"
    # 2026-07-06 02:00Z == 11:00 KST (target date 로컬 06일)
    records = [
        {"type": "user", "sessionId": "s1", "cwd": r"D:\demo\repo",
         "gitBranch": "main", "timestamp": "2026-07-06T02:00:00.000Z",
         "message": {"role": "user", "content": "로그인 버그 고쳐줘"}},
        {"type": "assistant", "sessionId": "s1", "cwd": r"D:\demo\repo",
         "timestamp": "2026-07-06T02:01:00.000Z",
         "message": {"role": "assistant", "model": "claude-opus-4-8",
                     "usage": {"output_tokens": 123},
                     "content": [
                         {"type": "text", "text": "고치겠습니다"},
                         {"type": "tool_use", "name": "Edit",
                          "input": {"file_path": r"D:\demo\repo\auth.py"}},
                         {"type": "tool_use", "name": "Bash",
                          "input": {"command": "pytest -q"}},
                     ]}},
        # tool_result 는 user 레코드지만 사람 입력이 아니므로 intent 로 잡히면 안 됨
        {"type": "user", "cwd": r"D:\demo\repo", "timestamp": "2026-07-06T02:01:05.000Z",
         "message": {"role": "user", "content": [
             {"type": "tool_result", "tool_use_id": "x", "content": "1 passed"}]}},
        {"type": "ai-title", "aiTitle": "로그인 버그 수정", "sessionId": "s1"},
    ]
    path = _write_session(str(sess_dir), records)
    # mtime 을 대상 날짜 범위 안으로 강제 (mtime 필터 통과 보장)
    ctx = _ctx(date(2026, 7, 6))
    os.utime(path, (ctx.start.timestamp() + 3600, ctx.start.timestamp() + 3600))

    coll = ClaudeLogCollector(ClaudeConfig(projects_dir=str(projects)))
    res = coll.collect(ctx)

    assert res.ok
    data = res.data
    assert data.total_sessions == 1
    s = data.sessions[0]
    assert s.intent == "로그인 버그 고쳐줘"
    assert s.title == "로그인 버그 수정"
    assert s.project == "repo"
    assert s.git_branch == "main"
    assert s.output_tokens == 123
    assert r"D:\demo\repo\auth.py" in s.files_edited
    assert s.tool_counts.get("Edit") == 1
    assert s.tool_counts.get("Bash") == 1
    assert any("pytest" in c for c in s.commands)
    assert data.cwds == [r"D:\demo\repo"]


def test_other_day_session_ignored(tmp_path):
    projects = tmp_path / "projects"
    sess_dir = projects / "D--demo-repo"
    records = [
        {"type": "user", "cwd": r"D:\demo\repo", "timestamp": "2026-07-01T02:00:00.000Z",
         "message": {"role": "user", "content": "어제 일"}},
    ]
    path = _write_session(str(sess_dir), records)
    ctx = _ctx(date(2026, 7, 6))
    # mtime 을 대상일로 맞춰도(파일은 열리지만) 그날 레코드가 없으므로 세션 0개여야 함
    os.utime(path, (ctx.start.timestamp() + 3600, ctx.start.timestamp() + 3600))

    coll = ClaudeLogCollector(ClaudeConfig(projects_dir=str(projects)))
    res = coll.collect(ctx)
    assert res.data.total_sessions == 0
