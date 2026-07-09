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
            # YYYY-MM-DD.md 는 옵시디언 데일리노트 파일명과 동일하다. subdir 가 비었거나
            # 데일리노트 폴더를 가리키면 사용자의 실제 노트를 덮어쓸 수 있으므로, 기존 파일이
            # '우리 업무일지'(frontmatter 표식)가 아니면 덮어쓰지 않는다.
            marker = "tags: [업무일지]"
            if path.exists():
                try:
                    existing = path.read_text(encoding="utf-8", errors="replace")[:400]
                except OSError:
                    existing = ""
                if marker not in existing:
                    return SinkResult.failure(
                        self.name,
                        f"같은 이름의 기존 노트({path.name})가 업무일지가 아니라 덮어쓰지 않았습니다. "
                        f"outputs.obsidian.subdir 를 데일리노트와 다른 폴더로 지정하세요.")
            frontmatter = (
                "---\n"
                f"date: {worklog.target_date.isoformat()}\n"
                f"{marker}\n"
                "---\n\n"
            )
            path.write_text(frontmatter + worklog.full_markdown, encoding="utf-8")
            return SinkResult.success(self.name, str(path))
        except OSError as e:
            return SinkResult.failure(self.name, str(e))
