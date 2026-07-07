"""NaverWorks 다중 캘린더 병합 · 응답 파싱 (네트워크 없이 monkeypatch)."""

from __future__ import annotations

from worklog.collectors.naverworks import NaverWorksCollector, _extract_list
from worklog.config import NaverWorksConfig
from worklog.models import CalendarEvent


def test_extract_list_variants():
    assert _extract_list({"calendarPersonals": [{"a": 1}]}) == [{"a": 1}]
    assert _extract_list([{"a": 1}]) == [{"a": 1}]
    assert _extract_list({"foo": [{"a": 1}]}) == [{"a": 1}]   # 키 몰라도 첫 list
    assert _extract_list({}) == []


def test_get_events_merges_selected_calendars(monkeypatch):
    coll = NaverWorksCollector(NaverWorksConfig(user_id="u", calendar_ids=["a", "b"]))
    calls = []

    def fake(token, ctx, cid):
        calls.append(cid)
        t = "2026-07-06T09:00:00+09:00" if cid == "a" else "2026-07-06T08:00:00+09:00"
        return [CalendarEvent(title="ev-" + cid, start=t, end=None, all_day=False)]

    monkeypatch.setattr(coll, "_fetch_calendar", fake)
    evs = coll._get_events("tok", None)
    assert calls == ["a", "b"]                       # 선택한 캘린더 각각 조회
    assert [e.title for e in evs] == ["ev-b", "ev-a"]  # start 기준 정렬(08<09)


def test_get_events_uses_default_when_none(monkeypatch):
    coll = NaverWorksCollector(NaverWorksConfig(user_id="u"))
    calls = []
    monkeypatch.setattr(coll, "_fetch_calendar", lambda t, c, cid: calls.append(cid) or [])
    coll._get_events("tok", None)
    assert calls == [None]                            # 선택 없으면 기본 캘린더 1회
