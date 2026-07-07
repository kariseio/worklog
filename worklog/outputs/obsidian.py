"""Obsidian vault 출력: <vault>/<subdir>/YYYY-MM-DD.md.

Obsidian 은 결국 로컬 Markdown 파일 폴더이므로 vault 안에 파일을 직접 쓴다.
파일 앞에 간단한 YAML frontmatter(태그/날짜)를 붙여 vault 에서 잘 검색되게 한다.
"""

from __future__ import annotations

from pathlib import Path

from ..config import ObsidianOutputConfig
from ..models import WorkLog
from .base import Sink, SinkResult


def test_connection(cfg: ObsidianOutputConfig) -> tuple[bool, str]:
    """vault 경로가 존재하고 하위 폴더에 쓰기 가능한지 실제로 확인."""
    if not cfg.vault_dir:
        return False, "vault 경로를 입력하세요."
    vault = Path(cfg.vault_dir).expanduser()
    if not vault.exists():
        return False, f"경로가 없습니다: {vault}"
    if not vault.is_dir():
        return False, f"폴더가 아닙니다: {vault}"
    try:
        out_dir = vault / cfg.subdir if cfg.subdir else vault
        out_dir.mkdir(parents=True, exist_ok=True)
        probe = out_dir / ".worklog_write_test"
        probe.write_text("ok", encoding="utf-8")
        probe.unlink()
    except OSError as e:
        return False, f"쓰기 실패: {e}"
    where = f"{vault.name}/{cfg.subdir}" if cfg.subdir else vault.name
    return True, f"연결됨 · '{where}' 에 쓰기 가능"


class ObsidianSink(Sink):
    name = "obsidian"

    def __init__(self, cfg: ObsidianOutputConfig):
        self.cfg = cfg

    def write(self, worklog: WorkLog) -> SinkResult:
        if not self.cfg.vault_dir:
            return SinkResult.failure(self.name, "outputs.obsidian.vault_dir 미설정")
        vault = Path(self.cfg.vault_dir).expanduser()
        if not vault.exists():
            return SinkResult.failure(self.name, f"vault 경로 없음: {vault}")
        try:
            out_dir = vault / self.cfg.subdir if self.cfg.subdir else vault
            out_dir.mkdir(parents=True, exist_ok=True)
            path = out_dir / f"{worklog.target_date.isoformat()}.md"
            frontmatter = (
                "---\n"
                f"date: {worklog.target_date.isoformat()}\n"
                "tags: [업무일지]\n"
                "---\n\n"
            )
            path.write_text(frontmatter + worklog.full_markdown, encoding="utf-8")
            return SinkResult.success(self.name, str(path))
        except OSError as e:
            return SinkResult.failure(self.name, str(e))
