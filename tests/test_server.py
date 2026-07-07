"""FastAPI 서버 엔드포인트 스모크 테스트. fastapi 미설치 시 skip."""

from __future__ import annotations

import pytest

fastapi = pytest.importorskip("fastapi")
testclient = pytest.importorskip("fastapi.testclient")


@pytest.fixture()
def client(tmp_path, monkeypatch):
    from fastapi.testclient import TestClient

    from worklog.webapp.server import create_app

    monkeypatch.setenv("WORKLOG_SETTINGS", str(tmp_path / "settings.json"))
    cfg = tmp_path / "config.yaml"
    out = tmp_path / "out"
    cfg.write_text(
        "timezone: Asia/Seoul\n"
        "sources:\n"
        "  git: {enabled: true}\n"
        "  claude: {enabled: false}\n"
        "  activitywatch: {enabled: false}\n"
        "  naverworks: {enabled: false}\n"
        "outputs:\n"
        f"  markdown: {{enabled: true, dir: {out.as_posix()}}}\n",
        encoding="utf-8",
    )
    return TestClient(create_app(str(cfg))), out


def test_status(client):
    c, _ = client
    r = c.get("/api/status")
    assert r.status_code == 200
    assert r.json()["timezone"] == "Asia/Seoul"


def test_collect_git_only(client):
    c, _ = client
    r = c.get("/api/collect", params={"date": "2026-07-06", "sources": "git"})
    assert r.status_code == 200
    body = r.json()
    assert body["date"] == "2026-07-06"
    assert "facts_markdown" in body
    names = {s["name"] for s in body["statuses"]}
    assert "git" in names


def test_save_and_history(client):
    c, out = client
    r = c.post("/api/save", json={
        "date": "2026-07-06",
        "summary_markdown": "## 한 줄 요약\n테스트 저장",
        "facts_markdown": "# facts",
        "targets": ["markdown"],
    })
    assert r.status_code == 200
    results = r.json()["results"]
    assert results and results[0]["ok"]
    saved = out / "2026-07-06.md"
    assert saved.exists()
    assert "테스트 저장" in saved.read_text(encoding="utf-8")

    h = c.get("/api/history")
    assert "2026-07-06" in h.json()["dates"]
