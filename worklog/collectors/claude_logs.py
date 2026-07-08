"""Claude Code 세션 로그 수집기.

~/.claude/projects/<sanitized>/<session-uuid>.jsonl 을 읽어
그날 무슨 작업을 했는지(세션 의도, 수정한 파일, 실행한 명령, 도구 사용량)를 뽑는다.

레코드 스키마 요약(실측):
  - type=user       : message.content = 문자열 또는 블록리스트. tool_result 블록은 도구 출력이므로 제외.
  - type=assistant  : message.content = [text | tool_use ...], message.usage.output_tokens, message.model
  - type=ai-title   : aiTitle (세션 요약 한 줄)  ← type="summary" 는 없음
  - 모든 user/assistant 레코드에 timestamp(UTC), cwd(실제 경로), gitBranch
  - type=file-history-snapshot : snapshot.trackedFileBackups 키 = 수정 대상 파일 경로들
"""

from __future__ import annotations

import glob
import json
import os

from ..config import ClaudeConfig
from ..models import ClaudeData, ClaudeSession
from ..util import parse_iso
from .base import CollectContext, Collector, CollectorResult

EDIT_TOOLS = {"Edit", "Write", "MultiEdit", "NotebookEdit"}
SHELL_TOOLS = {"Bash", "PowerShell"}


class ClaudeLogCollector(Collector):
    name = "claude"

    def __init__(self, cfg: ClaudeConfig):
        self.cfg = cfg

    def _projects_dir(self) -> str:
        if self.cfg.projects_dir:
            return os.path.expanduser(self.cfg.projects_dir)
        return os.path.expanduser(os.path.join("~", ".claude", "projects"))

    def collect(self, ctx: CollectContext) -> CollectorResult:
        projects = self._projects_dir()
        if not os.path.isdir(projects):
            return CollectorResult.skip(
                self.name, f"Claude 로그 폴더가 없습니다: {projects}"
            )

        # 그날 시작 이후에 수정된 파일만 열어 비용을 줄인다
        # (그날 활동이 있었다면 그 시점 이후에 파일이 쓰였으므로 mtime >= day start).
        day_start_ts = ctx.start.timestamp()

        sessions: list[ClaudeSession] = []
        warnings: list[str] = []
        files = glob.glob(os.path.join(projects, "*", "*.jsonl"))

        for path in files:
            try:
                if os.path.getmtime(path) < day_start_ts:
                    continue
            except OSError:
                continue
            try:
                session = self._parse_file(path, ctx)
            except Exception as e:  # noqa: BLE001
                warnings.append(f"세션 파싱 실패({os.path.basename(path)}): {e}")
                continue
            if session is not None:
                sessions.append(session)

        sessions.sort(key=lambda s: (s.first_ts is None, s.first_ts))
        return CollectorResult(
            name=self.name, data=ClaudeData(sessions=sessions), warnings=warnings
        )

    # ------------------------------------------------------------------ #

    def _parse_file(self, path: str, ctx: CollectContext) -> ClaudeSession | None:
        recs = []
        # errors="replace": 한 줄에 깨진 바이트가 있어도 세션 파일 전체가 드롭되지 않게.
        with open(path, encoding="utf-8", errors="replace") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    recs.append(json.loads(line))
                except json.JSONDecodeError:
                    continue

        target = ctx.target_date
        tz = ctx.tz

        def local_date(o):
            ts = o.get("timestamp") or (o.get("snapshot", {}) or {}).get("timestamp")
            dt = parse_iso(ts)
            return dt.astimezone(tz).date() if dt else None

        # 그날 활동이 하나도 없으면 건너뜀
        if not any(local_date(o) == target for o in recs):
            return None

        s = ClaudeSession(
            session_id=None, project=None, cwd=None, git_branch=None,
            title=None, intent=None,
        )
        files_edited: set[str] = set()
        files_read: set[str] = set()

        for o in recs:
            t = o.get("type")
            if o.get("sessionId") and not s.session_id:
                s.session_id = o["sessionId"]
            if o.get("cwd") and not s.cwd:
                s.cwd = o["cwd"]
                s.project = _project_name(o["cwd"])
            if o.get("gitBranch") and not s.git_branch:
                s.git_branch = o["gitBranch"]
            if t == "ai-title" and o.get("aiTitle"):
                s.title = o["aiTitle"]

            d = local_date(o)
            if d != target:
                continue

            ts = parse_iso(o.get("timestamp"))
            if ts:
                if s.first_ts is None or ts < s.first_ts:
                    s.first_ts = ts
                if s.last_ts is None or ts > s.last_ts:
                    s.last_ts = ts

            if t == "user":
                txt = _real_user_text(o)
                if txt and not s.intent:
                    s.intent = txt[: self.cfg.max_intent_len]
            elif t == "assistant":
                msg = o.get("message", {}) or {}
                usage = msg.get("usage", {}) or {}
                s.output_tokens += int(usage.get("output_tokens", 0) or 0)
                for b in msg.get("content", []) or []:
                    if not isinstance(b, dict) or b.get("type") != "tool_use":
                        continue
                    nm = b.get("name") or "?"
                    inp = b.get("input", {}) or {}
                    s.tool_counts[nm] = s.tool_counts.get(nm, 0) + 1
                    if nm in EDIT_TOOLS and inp.get("file_path"):
                        files_edited.add(inp["file_path"])
                    elif nm == "Read" and inp.get("file_path"):
                        files_read.add(inp["file_path"])
                    elif nm in SHELL_TOOLS and inp.get("command"):
                        s.commands.append(str(inp["command"])[:200])
            elif t == "file-history-snapshot":
                snap = o.get("snapshot", {}) or {}
                for p in (snap.get("trackedFileBackups") or {}):
                    files_edited.add(p)

        s.files_edited = sorted(files_edited)
        if self.cfg.include_read:
            s.files_read = sorted(files_read)
        return s


def _project_name(cwd: str) -> str:
    """cwd 로부터 표시용 프로젝트명. git worktree(.claude/worktrees/<name>)면 실제 저장소명으로 매핑."""
    norm = cwd.replace("\\", "/")
    marker = "/.claude/worktrees/"
    if marker in norm:
        norm = norm.split(marker, 1)[0]   # 실제 저장소 루트
    return norm.rstrip("/").rsplit("/", 1)[-1] or cwd


def _real_user_text(o: dict) -> str | None:
    """사용자 '진짜' 프롬프트만. tool_result / 시스템 주입 프롬프트는 제외."""
    if o.get("type") != "user" or o.get("isMeta"):
        return None
    content = (o.get("message", {}) or {}).get("content")
    if isinstance(content, str):
        text = content
    elif isinstance(content, list):
        parts = [b.get("text", "") for b in content if isinstance(b, dict) and b.get("type") == "text"]
        has_tool_result = any(isinstance(b, dict) and b.get("type") == "tool_result" for b in content)
        if has_tool_result and not any(parts):
            return None  # 도구 출력이지 사람 입력이 아님
        text = "\n".join(parts)
    else:
        return None
    text = (text or "").strip()
    if not text or text.startswith("<command-name>") or text.startswith("<system-reminder>"):
        return None
    return text
