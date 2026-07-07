"""로컬 Markdown 파일 출력: <dir>/YYYY-MM-DD.md."""

from __future__ import annotations

from pathlib import Path

from ..config import MarkdownOutputConfig
from ..models import WorkLog
from .base import Sink, SinkResult


class MarkdownSink(Sink):
    name = "markdown"

    def __init__(self, cfg: MarkdownOutputConfig):
        self.cfg = cfg

    def write(self, worklog: WorkLog) -> SinkResult:
        try:
            out_dir = Path(self.cfg.dir).expanduser()
            out_dir.mkdir(parents=True, exist_ok=True)
            path = out_dir / f"{worklog.target_date.isoformat()}.md"
            path.write_text(worklog.full_markdown, encoding="utf-8")
            return SinkResult.success(self.name, str(path))
        except OSError as e:
            return SinkResult.failure(self.name, str(e))
