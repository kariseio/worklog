"""리뷰 확정 결함 수정 회귀 테스트 (2차)."""

from __future__ import annotations

import json
import os
from datetime import date

import pytest


# ── [6] save_app_settings 원자적 쓰기 + 손상 복구 ──────────────────────────
def test_settings_atomic_save_and_corrupt_recovery(tmp_path, monkeypatch):
    from worklog import config

    p = tmp_path / "settings.json"
    monkeypatch.setenv("WORKLOG_SETTINGS", str(p))

    config.save_app_settings({"a": 1, "secret": "keepme"})
    assert config.load_app_settings() == {"a": 1, "secret": "keepme"}
    assert not (tmp_path / "settings.json.tmp").exists()   # 임시파일 남지 않음

    # 부분 쓰기로 손상 → load 는 {} 반환하되 원본을 .bak 로 보존(조용한 전소실 방지)
    p.write_text("{ broken json", encoding="utf-8")
    assert config.load_app_settings() == {}
    assert (tmp_path / "settings.json.bak").exists()


# ── [2] 스캔 루트 pruning: 미마운트 드라이브는 보존 ───────────────────────
def test_root_definitely_gone_basic(tmp_path):
    from worklog.webapp.server import _root_definitely_gone

    assert _root_definitely_gone(str(tmp_path)) is False            # 존재 → 삭제 아님
    assert _root_definitely_gone(str(tmp_path / "nope")) is True     # 폴더 없음(드라이브 있음)


@pytest.mark.skipif(os.name != "nt", reason="드라이브 앵커는 Windows 전용")
def test_root_kept_when_drive_unmounted(monkeypatch):
    from worklog.webapp import server

    # 폴더도 없고 드라이브 앵커도 없음(외장 SSD 분리 등) → 보존(False)
    monkeypatch.setattr(server.os.path, "isdir", lambda _p: False)
    monkeypatch.setattr(server.os.path, "exists", lambda _p: False)
    assert server._root_definitely_gone("E:\\repos") is False


# ── [3] render_facts 도 meta(자동요약) 세션을 제외 ───────────────────────
def test_render_facts_excludes_meta_sessions():
    from worklog.models import ClaudeData, ClaudeSession, DailyData
    from worklog.render import WORKLOG_SENTINEL, render_facts
    from worklog.util import get_tz

    normal = ClaudeSession(session_id="1", project="proj", cwd="/x", git_branch=None,
                           title="기능 작업", intent="기능 추가", output_tokens=100)
    meta = ClaudeSession(session_id="2", project="proj", cwd="/x", git_branch=None,
                         title=None, intent=WORKLOG_SENTINEL + " 요약 실행", output_tokens=5000)
    data = DailyData(target_date=date(2026, 7, 8), tz_name="Asia/Seoul")
    data.claude = ClaudeData(sessions=[normal, meta])

    md = render_facts(data, get_tz("Asia/Seoul"))
    assert "1세션" in md            # meta 제외 → 1세션
    assert "2세션" not in md
    assert "5,000" not in md        # meta 토큰(5000) 미포함


# ── [4] NaverWorks 캘린더 이벤트 페이지네이션 ────────────────────────────
def test_naverworks_event_pagination(monkeypatch):
    from worklog.collectors import naverworks
    from worklog.collectors.base import CollectContext
    from worklog.collectors.naverworks import NaverWorksCollector
    from worklog.config import NaverWorksConfig
    from worklog.util import get_tz, resolve_day

    calls = {"n": 0, "cursors": []}

    class _Resp:
        status_code = 200

        def __init__(self, payload):
            self._p = payload

        def json(self):
            return self._p

    def fake_get(url, headers=None, params=None, timeout=None):
        calls["n"] += 1
        calls["cursors"].append((params or {}).get("cursor"))
        if not (params or {}).get("cursor"):
            return _Resp({"events": [], "responseMetaData": {"nextCursor": "PAGE2"}})
        return _Resp({"events": [], "responseMetaData": {}})   # 마지막 페이지

    monkeypatch.setattr(naverworks.requests, "get", fake_get)

    tz = get_tz("Asia/Seoul")
    t, start, end = resolve_day("2026-07-08", tz)
    import logging
    ctx = CollectContext(target_date=t, start=start, end=end, tz=tz,
                         tz_name="Asia/Seoul", logger=logging.getLogger("test"))
    coll = NaverWorksCollector(NaverWorksConfig(enabled=True, user_id="me@x"))
    coll._fetch_calendar("tok", ctx, "cal1")

    assert calls["n"] == 2                        # 2페이지 모두 조회(예전엔 1페이지만)
    assert calls["cursors"] == [None, "PAGE2"]    # 첫 페이지→cursor 없음, 둘째→nextCursor
