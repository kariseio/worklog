"""앱 설정 저장/불러오기 + 연결 테스트. fastapi 미설치 시 skip."""

from __future__ import annotations

import json

import pytest

pytest.importorskip("fastapi")
from fastapi.testclient import TestClient  # noqa: E402

from worklog.webapp.server import create_app  # noqa: E402


@pytest.fixture()
def client(tmp_path, monkeypatch):
    monkeypatch.setenv("WORKLOG_SETTINGS", str(tmp_path / "settings.json"))
    conf = tmp_path / "config.yaml"
    out = tmp_path / "out"
    conf.write_text(
        "timezone: Asia/Seoul\n"
        f"outputs:\n  markdown: {{enabled: true, dir: {out.as_posix()}}}\n",
        encoding="utf-8",
    )
    return TestClient(create_app(str(conf))), tmp_path


def test_settings_roundtrip_and_mask(client):
    c, tmp = client
    assert c.get("/api/settings").json()["notion"]["token_set"] is False

    r = c.post("/api/settings", json={"notion": {
        "enabled": True, "parent_type": "page", "parent_id": "pid",
        "title_prop": "Name", "token": "ntn_secret",
    }})
    assert r.json()["ok"]

    g = c.get("/api/settings").json()
    assert g["notion"]["token_set"] is True         # set 플래그만
    assert "token" not in g["notion"]               # 원값은 노출 안 됨
    assert g["notion"]["parent_id"] == "pid"

    store = json.loads((tmp / "settings.json").read_text(encoding="utf-8"))
    assert store["outputs"]["notion"]["token"] == "ntn_secret"


def test_blank_secret_keeps_existing(client):
    c, tmp = client
    c.post("/api/settings", json={"notion": {"enabled": True, "parent_id": "p1", "token": "ntn_A"}})
    c.post("/api/settings", json={"notion": {"enabled": True, "parent_id": "p2", "token": ""}})
    store = json.loads((tmp / "settings.json").read_text(encoding="utf-8"))
    assert store["outputs"]["notion"]["token"] == "ntn_A"   # 빈 값이면 기존 유지
    assert store["outputs"]["notion"]["parent_id"] == "p2"  # 비밀 아닌 값은 갱신


def test_obsidian_test_and_real_save(client):
    c, tmp = client
    vault = tmp / "vault"
    vault.mkdir()

    assert c.post("/api/test/obsidian", json={"vault_dir": str(vault), "subdir": "업무일지"}).json()["ok"]
    assert c.post("/api/test/obsidian", json={"vault_dir": str(tmp / "nope"), "subdir": "x"}).json()["ok"] is False

    c.post("/api/settings", json={"obsidian": {"enabled": True, "vault_dir": str(vault), "subdir": "업무일지"}})
    sr = c.post("/api/save", json={
        "date": "2026-07-06", "summary_markdown": "## 요약\n연동 저장",
        "facts_markdown": "# f", "targets": ["obsidian"],
    })
    results = sr.json()["results"]
    assert results and results[0]["ok"], results
    saved = vault / "업무일지" / "2026-07-06.md"
    assert saved.exists()
    assert "연동 저장" in saved.read_text(encoding="utf-8")


def test_connection_tests_without_creds_do_not_crash(client):
    c, _ = client
    assert c.post("/api/test/notion", json={"parent_id": "x"}).json()["ok"] is False   # 토큰 없음
    assert c.post("/api/test/naverworks", json={}).json()["ok"] is False                # 자격증명 없음


def test_env_naverworks_not_clobbered_by_blank_ui(tmp_path, monkeypatch):
    """UI 에서 빈 칸으로 저장해도 .env 로 넣은 NaverWorks 자격증명이 지워지면 안 된다."""
    from worklog.config import load_config, save_app_settings

    monkeypatch.setenv("WORKLOG_SETTINGS", str(tmp_path / "settings.json"))
    monkeypatch.setenv("NAVERWORKS_CLIENT_ID", "env-cid")
    monkeypatch.setenv("NAVERWORKS_SERVICE_ACCOUNT", "env-sa@dom")

    # 앱에서 무관한 항목을 저장 → naverworks 자격증명 칸은 비어서 "" 로 기록됨
    save_app_settings({"sources": {"naverworks": {
        "enabled": True, "client_id": "", "service_account": "", "private_key_path": "",
    }}})

    cfg = load_config(str(tmp_path / "config.yaml"))  # config.yaml 없어도 됨
    assert cfg.naverworks.client_id == "env-cid"          # .env 값 유지
    assert cfg.naverworks.service_account == "env-sa@dom"  # .env 값 유지
    assert cfg.naverworks.enabled is True                 # bool 은 정상 반영


def test_naverworks_calendar_ids_roundtrip(client):
    """여러 캘린더 선택이 저장/조회되고 config 에 반영되는지."""
    from worklog.config import load_config

    c, tmp = client
    r = c.post("/api/settings", json={"naverworks": {
        "enabled": True, "user_id": "u@x",
        "calendar_ids": ["id1", "id2"],
        "calendars": [{"calendar_id": "id1", "name": "내 캘린더"},
                      {"calendar_id": "id2", "name": "팀"}],
    }})
    assert r.json()["ok"]

    g = c.get("/api/settings").json()["naverworks"]
    assert g["calendar_ids"] == ["id1", "id2"]
    assert {x["calendar_id"] for x in g["calendars"]} == {"id1", "id2"}

    cfg = load_config(None)  # WORKLOG_SETTINGS(temp) 오버레이 적용
    assert cfg.naverworks.calendar_ids == ["id1", "id2"]


def test_notion_version_preserved_on_ui_save(client):
    """UI 는 version 을 안 보내므로, 저장 시 커스텀 version 이 기본값으로 덮이면 안 된다."""
    c, tmp = client
    c.post("/api/settings", json={"notion": {"enabled": True, "parent_id": "p", "version": "2022-06-28"}})
    c.post("/api/settings", json={"notion": {"enabled": True, "parent_id": "p2"}})  # version 미포함
    store = json.loads((tmp / "settings.json").read_text(encoding="utf-8"))
    assert store["outputs"]["notion"]["version"] == "2022-06-28"
    assert store["outputs"]["notion"]["parent_id"] == "p2"
