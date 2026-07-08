"""자동 업데이트 로직 — 버전 비교 + GitHub 릴리스 확인(네트워크는 목킹)."""

from __future__ import annotations

import os

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


def test_updater_bat_swaps_then_settles_then_relaunches():
    bat = update._UPDATER_BAT
    # 교체(move) → settle delay(ping) → 자동 재실행(start) 순서가 모두 있어야 한다.
    assert "move /y" in bat
    assert "start" in bat
    i_swap = bat.index(":swapped")
    # settle delay(ping)와 start 는 교체 성공(:swapped) 이후에 온다.
    assert bat.index("ping", i_swap) < bat.index("start", i_swap)


@pytest.mark.skipif(os.name != "nt", reason="콘솔창 숨김 플래그는 Windows 전용")
def test_updater_uses_hidden_console_not_detached():
    # CREATE_NO_WINDOW(숨김 콘솔) 를 써서 배치의 ping/move 창이 뜨지 않게 한다.
    # DETACHED_PROCESS(콘솔 제거)면 자식 콘솔앱이 새 창을 띄우므로 쓰지 않는다.
    assert update._UPDATER_FLAGS & 0x08000000        # CREATE_NO_WINDOW
    assert not (update._UPDATER_FLAGS & 0x00000008)  # not DETACHED_PROCESS


def test_cleanup_noop_when_not_frozen(monkeypatch):
    monkeypatch.setattr(update, "is_frozen", lambda: False)
    assert update.cleanup_stale_extractions() == 0


@pytest.mark.skipif(os.name != "nt", reason="onefile 추출폴더 청소는 Windows 전용")
def test_cleanup_removes_only_signed_stale_dirs(monkeypatch, tmp_path):
    monkeypatch.setattr(update, "is_frozen", lambda: True)
    monkeypatch.setattr(update.tempfile, "gettempdir", lambda: str(tmp_path))
    # 현재 실행 중인 폴더로 위장(보존돼야 함)
    mine = tmp_path / "_MEI_current"
    (mine / "worklog" / "webapp" / "static").mkdir(parents=True)
    monkeypatch.setattr(update.sys, "_MEIPASS", str(mine), raising=False)
    # worklog 서명이 있는 누출 폴더(지워져야 함)
    stale = tmp_path / "_MEI999999"
    (stale / "worklog" / "webapp" / "static").mkdir(parents=True)
    # 서명 없는 타 앱 폴더(보존)와 _MEI 아닌 폴더(무시)
    other = tmp_path / "_MEI888888"
    (other / "other_app").mkdir(parents=True)
    notmei = tmp_path / "randomdir"
    notmei.mkdir()

    removed = update.cleanup_stale_extractions()
    assert removed == 1
    assert not stale.exists()   # 지워짐
    assert mine.exists()        # 현재 폴더 보존
    assert other.exists()       # 서명 없어 보존(타 앱 보호)
    assert notmei.exists()
