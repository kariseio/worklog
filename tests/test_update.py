"""자동 업데이트 로직 — 버전 비교 + GitHub 릴리스 확인(네트워크는 목킹)."""

from __future__ import annotations

import pytest

from worklog import update


def test_version_parse_and_compare():
    assert update._parse("0.1.3") == (0, 1, 3)
    assert update._parse("v0.1.10") == (0, 1, 10)
    assert update._parse("1.2") == (1, 2)
    assert update.is_newer("0.1.3", "0.1.2") is True
    assert update.is_newer("0.1.10", "0.1.9") is True     # 숫자 비교(문자열 아님)
    assert update.is_newer("0.1.2", "0.1.2") is False
    assert update.is_newer("0.1.1", "0.1.2") is False
    assert update.is_newer("1.0.0", "0.9.9") is True


def test_pick_exe_asset_prefers_worklog_exe():
    assets = [
        {"name": "notes.txt", "browser_download_url": "u0"},
        {"name": "worklog-0.1.3.exe", "browser_download_url": "u1"},
        {"name": "worklog.exe", "browser_download_url": "u2"},
    ]
    assert update._pick_exe_asset(assets) == "u2"          # worklog.exe 우선
    assert update._pick_exe_asset([{"name": "a.exe", "browser_download_url": "x"}]) == "x"
    assert update._pick_exe_asset([{"name": "readme.md"}]) is None
    assert update._pick_exe_asset([]) is None


class _Resp:
    def __init__(self, status, payload):
        self.status_code = status
        self._payload = payload

    def json(self):
        return self._payload


def test_check_reports_update(monkeypatch):
    payload = {
        "tag_name": "v99.0.0",
        "body": "새 기능",
        "assets": [{"name": "worklog.exe", "browser_download_url": "https://x/worklog.exe"}],
    }
    monkeypatch.setattr(update, "__version__", "0.1.2", raising=False)
    import requests
    monkeypatch.setattr(requests, "get", lambda *a, **k: _Resp(200, payload))

    r = update.check()
    assert r["latest"] == "99.0.0"
    assert r["update_available"] is True
    assert r["download_url"] == "https://x/worklog.exe"
    assert r["error"] is None


def test_check_no_update_when_same_version(monkeypatch):
    payload = {"tag_name": "v0.1.2", "assets": [{"name": "worklog.exe", "browser_download_url": "u"}]}
    monkeypatch.setattr(update, "__version__", "0.1.2", raising=False)
    import requests
    monkeypatch.setattr(requests, "get", lambda *a, **k: _Resp(200, payload))
    assert update.check()["update_available"] is False


def test_check_handles_no_release(monkeypatch):
    import requests
    monkeypatch.setattr(requests, "get", lambda *a, **k: _Resp(404, {"message": "Not Found"}))
    r = update.check()
    assert r["update_available"] is False
    assert r["error"] == "HTTP 404"


def test_check_network_error_is_captured(monkeypatch):
    import requests

    def _boom(*a, **k):
        raise requests.ConnectionError("no net")

    monkeypatch.setattr(requests, "get", _boom)
    r = update.check()
    assert r["update_available"] is False
    assert "no net" in (r["error"] or "")
