"""전체 리뷰에서 확인된 결함들의 회귀 테스트."""

from __future__ import annotations

import os

from worklog import service
from worklog.config import Config
from worklog.outputs.notion import markdown_to_blocks


def test_default_markdown_dir_is_documents():
    from worklog.config import documents_dir

    docs = documents_dir()
    assert docs and os.path.isabs(docs)
    cfg = Config()
    d = cfg.outputs.markdown.dir.replace("\\", "/")
    assert d.startswith(docs.replace("\\", "/"))   # 문서 폴더 아래
    assert d.endswith("업무일지")


# --- #12 코드블록 원문 보존 ---
def test_code_block_preserves_inline_markdown():
    blocks = markdown_to_blocks("```python\ny = '**b**' + `c` + '[x](u)'\n```\n")
    code = next(b for b in blocks if b["type"] == "code")
    content = "".join(rt["text"]["content"] for rt in code["code"]["rich_text"])
    assert "**b**" in content and "`c`" in content and "[x](u)" in content


# --- #16 divider 오판 ---
def test_dashes_with_text_is_not_divider():
    blocks = markdown_to_blocks("--- 중요: 이건 divider 가 아님\n")
    assert not any(b["type"] == "divider" for b in blocks)
    assert any(b["type"] == "paragraph" for b in blocks)


def test_pure_dashes_is_divider():
    assert any(b["type"] == "divider" for b in markdown_to_blocks("---\n"))


# --- #15 경로 traversal 차단 ---
def test_read_saved_rejects_traversal(tmp_path):
    (tmp_path / "outside.md").write_text("SECRET", encoding="utf-8")
    logs = tmp_path / "logs"
    logs.mkdir()
    (logs / "2026-07-06.md").write_text("ok", encoding="utf-8")
    cfg = Config()
    cfg.outputs.markdown.dir = str(logs)

    assert service.read_saved(cfg, "2026-07-06") == "ok"
    assert service.read_saved(cfg, "../outside") is None
    assert service.read_saved(cfg, "..\\outside") is None
    assert service.read_saved(cfg, "2026-07-06.md") is None   # 형식 불일치


# --- #14 빈 env 가 시크릿을 덮지 않음 ---
def test_empty_env_does_not_clobber_secret(tmp_path, monkeypatch):
    from worklog.config import load_config, save_app_settings

    monkeypatch.setenv("WORKLOG_SETTINGS", str(tmp_path / "s.json"))
    save_app_settings({"sources": {"naverworks": {"enabled": True, "client_secret": "stored"}}})
    monkeypatch.setenv("NAVERWORKS_CLIENT_SECRET", "")   # export 됐지만 빈 값
    cfg = load_config(str(tmp_path / "none.yaml"))
    assert cfg.naverworks.client_secret == "stored"


# --- #9 앱에서 (비밀 아닌) 값 지우기 반영 ---
def test_app_can_clear_nonsecret_value(tmp_path, monkeypatch):
    from worklog.config import load_config, save_app_settings

    monkeypatch.setenv("WORKLOG_SETTINGS", str(tmp_path / "s.json"))
    conf = tmp_path / "c.yaml"
    conf.write_text(
        "sources: {}\noutputs:\n  obsidian: {enabled: true, vault_dir: /old/vault, subdir: X}\n",
        encoding="utf-8",
    )
    save_app_settings({"outputs": {"obsidian": {"enabled": False, "vault_dir": "", "subdir": "X"}}})
    cfg = load_config(str(conf))
    assert cfg.outputs.obsidian.vault_dir == ""       # config.yaml 값이 앱에서 지워짐
    assert cfg.outputs.obsidian.enabled is False


# --- #6 --init 템플릿이 패키지에 포함됨 ---
def test_init_templates_exist():
    from worklog.config import EXAMPLE_CONFIG_PATH, EXAMPLE_ENV_PATH

    assert EXAMPLE_CONFIG_PATH.exists()
    assert EXAMPLE_ENV_PATH.exists()


# --- #17 잘못된 --date 는 traceback 대신 종료코드 2 ---
def test_cli_invalid_date_returns_2(tmp_path, monkeypatch):
    from worklog.cli import main

    monkeypatch.setenv("WORKLOG_SETTINGS", str(tmp_path / "s.json"))
    assert main(["--date", "2026-99-99", "--dry-run"]) == 2
