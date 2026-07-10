"""OpenAI Codex CLI 롤아웃 세션 로그 수집기.

Codex 는 세션 전체를 JSONL '롤아웃' 파일로 남긴다(= source of truth). 위치:
  ${CODEX_HOME|~/.codex}/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl[.zst]
state_5.sqlite 는 이 파일들을 가리키는 '인덱스'일 뿐이라(드리프트·손상 이슈 있음)
읽지 않고 롤아웃 파일을 직접 스트리밍 파싱한다.

롤아웃 라인 스키마(실측·GitHub 소스 검증):
  {"timestamp":"<RFC3339 UTC ms, Z>", "type":"<kind>", "payload":{...}}
  - type=session_meta   : payload = SessionMeta(flatten) + git. id/session_id, cwd,
                          cli_version, model_provider, git.branch/commit_hash/repository_url
  - type=response_item  : payload.type ∈ message|reasoning|function_call|
                          function_call_output|local_shell_call|custom_tool_call ...
      message: {role:"user"|"assistant"|"developer"|"system",
                content:[{type:"input_text"|"output_text", text}]}
  - type=turn_context   : payload.model (구체 모델명), cwd 등
  - type=event_msg      : payload.type=token_count → info.total_token_usage(누적)
  - type=compacted|world_state|inter_agent_* : 무시

세션당 항목은 ClaudeSession(agent="codex") 로 담는다(렌더/분석 재사용).
"""

from __future__ import annotations

import glob
import io
import json
import os
import re

from ..config import CodexConfig
from ..models import ClaudeSession, CodexData, QATurn
from ..util import parse_iso
from .base import CollectContext, Collector, CollectorResult
from .claude_logs import _project_name

# Codex 가 세션 시작에 '사용자 메시지'로 주입하는 합성 래퍼(사람 입력이 아님).
_WRAPPERS = ("<environment_context>", "<user_instructions>", "<INSTRUCTIONS>")
# 셸/도구 실행 항목의 payload.type (버전별 명칭 차이 흡수).
_TOOL_CALL_TYPES = ("function_call", "local_shell_call", "custom_tool_call")
# apply_patch 패치 본문의 파일 경로 마커. rename 은 'Update File:'(원본) + 'Move to:'(대상) 로 나온다.
_PATCH_FILE_RE = re.compile(
    r"^\*\*\* (?:Add|Update|Delete) File: (.+)$|^\*\*\* Move to: (.+)$", re.MULTILINE)
_LINE_CAP = 2_000_000   # 한 줄 길이 상한(거대한 tool 출력 라인 방어)


class CodexCollector(Collector):
    name = "codex"

    def __init__(self, cfg: CodexConfig):
        self.cfg = cfg

    def _sessions_dir(self) -> str:
        if self.cfg.sessions_dir:
            return os.path.expanduser(self.cfg.sessions_dir)
        # Codex 는 CODEX_HOME(있으면) → ~/.codex 를 설정 루트로 쓴다.
        base = os.environ.get("CODEX_HOME") or os.path.join("~", ".codex")
        return os.path.join(os.path.expanduser(base), "sessions")

    def collect(self, ctx: CollectContext) -> CollectorResult:
        root = self._sessions_dir()
        if not os.path.isdir(root):
            return CollectorResult.skip(self.name, f"Codex 세션 폴더가 없습니다: {root}")

        day_start_ts = ctx.start.timestamp()
        sessions: list[ClaudeSession] = []
        warnings: list[str] = []

        # rollout-*.jsonl 과 rollout-*.jsonl.zst(압축·아카이브) 모두.
        files = glob.glob(os.path.join(root, "**", "rollout-*.jsonl*"), recursive=True)
        for path in files:
            if not (path.endswith(".jsonl") or path.endswith(".jsonl.zst")):
                continue
            if not os.path.isfile(path):
                continue
            try:
                if os.path.getmtime(path) < day_start_ts:
                    continue   # 그날 시작 전에 마지막으로 쓰인 파일은 그날 활동이 없다
            except OSError:
                continue
            try:
                s = self._parse(path, ctx, warnings)
            except Exception as e:  # noqa: BLE001
                warnings.append(f"세션 파싱 실패({os.path.basename(path)}): {e}")
                continue
            if s is not None:
                sessions.append(s)

        sessions.sort(key=lambda s: (s.first_ts is None, s.first_ts))
        return CollectorResult(
            name=self.name, data=CodexData(sessions=sessions), warnings=warnings
        )

    # ------------------------------------------------------------------ #

    def _open(self, path: str, warnings: list[str]):
        """롤아웃 파일을 텍스트 스트림으로 연다(.zst 는 zstandard 로 해제)."""
        if path.endswith(".zst"):
            try:
                import zstandard   # 선택 의존성: 없으면 압축 롤아웃만 건너뛴다
            except ImportError:
                warnings.append(f"zstandard 미설치로 압축 롤아웃 건너뜀: {os.path.basename(path)}")
                return None
            fh = None
            try:
                fh = open(path, "rb")
                reader = zstandard.ZstdDecompressor().stream_reader(fh)
                return io.TextIOWrapper(reader, encoding="utf-8", errors="replace")
            except Exception as e:  # noqa: BLE001
                if fh is not None:
                    try:
                        fh.close()
                    except OSError:
                        pass
                warnings.append(f"압축 해제 실패({os.path.basename(path)}): {e}")
                return None
        try:
            return open(path, encoding="utf-8", errors="replace")
        except OSError as e:
            warnings.append(f"열기 실패({os.path.basename(path)}): {e}")
            return None

    def _parse(self, path: str, ctx: CollectContext, warnings: list[str]) -> ClaudeSession | None:
        fh = self._open(path, warnings)
        if fh is None:
            return None

        tz = ctx.tz
        target = ctx.target_date

        s = ClaudeSession(
            session_id=None, project=None, cwd=None, git_branch=None,
            title=None, intent=None, agent="codex",
        )
        files_edited: set[str] = set()
        files_read: set[str] = set()

        qa_turns: list[QATurn] = []
        cur_qa: dict | None = None
        carry: dict | None = None      # 자정을 넘긴 답을 위해 직전(전날 포함) 사용자 프롬프트 보관
        matched = False                # 그날 활동이 하나라도 있었는지
        last_total_out: int | None = None   # 그날 마지막 token_count 의 누적 출력 토큰

        def _flush_qa():
            nonlocal cur_qa
            if cur_qa is not None:
                ans = " ".join(" ".join(cur_qa["a"]).split())
                qa_turns.append(QATurn(
                    time=cur_qa["time"], question=cur_qa["q"],
                    answer=ans[: self.cfg.max_answer_len],
                ))
                cur_qa = None

        try:
            for i, raw in enumerate(fh):
                if i >= self.cfg.max_lines:
                    warnings.append(f"행 상한({self.cfg.max_lines}) 초과로 일부 생략: {os.path.basename(path)}")
                    break
                line = raw.strip()
                if not line or len(line) > _LINE_CAP:
                    continue
                try:
                    o = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if not isinstance(o, dict):
                    continue
                typ = o.get("type")
                payload = o.get("payload")
                if not isinstance(payload, dict):
                    payload = {}

                # --- 날짜 무관 헤더/컨텍스트 ---
                if typ == "session_meta":
                    if not s.session_id:
                        s.session_id = payload.get("session_id") or payload.get("id")
                    cwd = payload.get("cwd")
                    if cwd and not s.cwd:
                        s.cwd = cwd
                        s.project = _project_name(cwd)
                    git = payload.get("git")
                    if isinstance(git, dict) and git.get("branch") and not s.git_branch:
                        s.git_branch = git.get("branch")
                    continue
                if typ == "turn_context":
                    continue   # 환경 스냅샷 — 수집 대상 필드 없음

                ts = parse_iso(o.get("timestamp"))
                d = ts.astimezone(tz).date() if ts else None

                # carry: 날짜 무관, 마지막 '진짜' 사용자 프롬프트 기억(자정 연속 대비)
                if typ == "response_item" and payload.get("type") == "message" \
                        and payload.get("role") == "user":
                    _utxt = _real_codex_user_text(payload)
                    if _utxt:
                        carry = {"time": ts.astimezone(tz).strftime("%H:%M") if ts else "",
                                 "q": _utxt[: self.cfg.max_intent_len]}

                if d != target:
                    continue
                matched = True
                if ts:
                    if s.first_ts is None or ts < s.first_ts:
                        s.first_ts = ts
                    if s.last_ts is None or ts > s.last_ts:
                        s.last_ts = ts

                if typ == "response_item":
                    pt = payload.get("type")
                    if pt == "message":
                        role = payload.get("role")
                        if role == "user":
                            txt = _real_codex_user_text(payload)
                            if txt:
                                _flush_qa()
                                hm = ts.astimezone(tz).strftime("%H:%M") if ts else ""
                                cur_qa = {"time": hm, "q": txt[: self.cfg.max_intent_len], "a": []}
                                if not s.intent:
                                    s.intent = txt[: self.cfg.max_intent_len]
                        elif role == "assistant":
                            if cur_qa is None and carry is not None:
                                cur_qa = {"time": carry["time"], "q": carry["q"], "a": []}
                                if not s.intent:
                                    s.intent = carry["q"]
                            text = _output_text(payload)
                            if text and cur_qa is not None:
                                if sum(len(p) for p in cur_qa["a"]) < self.cfg.max_answer_len * 3:
                                    cur_qa["a"].append(text)
                        # developer/system role 은 intent 대상이 아니므로 무시
                    elif pt in _TOOL_CALL_TYPES:
                        nm = payload.get("name") or ("shell" if pt == "local_shell_call" else "?")
                        s.tool_counts[nm] = s.tool_counts.get(nm, 0) + 1
                        cmd_full = _command_text(payload)   # 셸 argv 전체(비절삭)
                        if cmd_full:
                            s.commands.append(cmd_full[:200])
                        # 수정 파일 추출 — apply_patch 는 두 경로로 온다:
                        #  (1) 전용 apply_patch 툴 → 패치가 input/arguments 에.
                        #  (2) 셸로 실행된 apply_patch → 패치가 command 본문에 (name='shell' 등).
                        patch = ""
                        if nm == "apply_patch":
                            patch = _apply_patch_text(payload)
                        elif cmd_full and ("apply_patch" in cmd_full or "*** Begin Patch" in cmd_full):
                            patch = cmd_full
                        for f in _patch_files_from(patch):
                            files_edited.add(f)
                    # reasoning / function_call_output(원시 도구 출력) 등은 스킵
                elif typ == "event_msg":
                    if payload.get("type") == "token_count":
                        # total_token_usage 는 세션 시작부터의 '누적' 스냅샷 → 합산하지 말고
                        # 그날 마지막 값을 쓴다(중복합산 방지). 한계: 세션이 자정을 넘긴 경우
                        # 이 누적값엔 전날 토큰도 포함돼 소폭 과다 계상될 수 있다(소프트 지표라 허용).
                        info = payload.get("info")
                        if isinstance(info, dict):
                            tot = info.get("total_token_usage")
                            if isinstance(tot, dict) and tot.get("output_tokens") is not None:
                                try:
                                    last_total_out = int(tot.get("output_tokens") or 0)
                                except (TypeError, ValueError):
                                    pass
                # compacted / world_state / inter_agent_* 무시
        finally:
            try:
                fh.close()
            except Exception:  # noqa: BLE001
                pass

        if not matched:
            return None

        _flush_qa()
        if last_total_out is not None:
            s.output_tokens = last_total_out
        if len(qa_turns) > self.cfg.max_qa_turns:
            s.qa_dropped = len(qa_turns) - self.cfg.max_qa_turns
            qa_turns = qa_turns[-self.cfg.max_qa_turns:]   # tail 유지(하루 끝 결과 보존)
        s.qa = qa_turns
        s.files_edited = sorted(files_edited)
        if self.cfg.include_read:
            s.files_read = sorted(files_read)
        return s


# --------------------------------------------------------------------------- #
# 파싱 헬퍼
# --------------------------------------------------------------------------- #


def _real_codex_user_text(payload: dict) -> str | None:
    """message payload 에서 사람의 '진짜' 프롬프트 텍스트만. 합성 래퍼는 제외."""
    content = payload.get("content")
    if isinstance(content, str):
        text = content
    elif isinstance(content, list):
        parts = [b.get("text", "") for b in content
                 if isinstance(b, dict) and b.get("type") == "input_text"]
        text = "\n".join(p for p in parts if p)
    else:
        return None
    text = (text or "").strip()
    if not text:
        return None
    if text.lstrip().startswith(_WRAPPERS):
        return None   # <environment_context> 등 세션 시작 주입 메시지
    return text


def _output_text(payload: dict) -> str:
    """assistant message payload 에서 output_text 를 이어붙인 프로즈."""
    content = payload.get("content")
    if isinstance(content, str):
        return content.strip()
    if isinstance(content, list):
        parts = [b.get("text", "") for b in content
                 if isinstance(b, dict) and b.get("type") == "output_text"]
        return "\n".join(p for p in parts if p).strip()
    return ""


def _tool_args(payload: dict) -> dict | None:
    """function_call.arguments(문자열 JSON) 또는 local_shell_call.action 을 dict 로."""
    args = payload.get("arguments")
    if isinstance(args, str):
        try:
            obj = json.loads(args)
        except (json.JSONDecodeError, ValueError):
            return None
        return obj if isinstance(obj, dict) else None
    if isinstance(args, dict):
        return args
    action = payload.get("action")
    return action if isinstance(action, dict) else None


def _command_text(payload: dict) -> str | None:
    """셸 계열 도구 호출의 실행 커맨드 전체 문자열(비절삭, best-effort)."""
    obj = _tool_args(payload)
    if not isinstance(obj, dict):
        return None
    cmd = obj.get("command")
    if isinstance(cmd, list):
        return " ".join(str(x) for x in cmd) or None
    if isinstance(cmd, str) and cmd.strip():
        return cmd.strip()
    return None


def _apply_patch_text(payload: dict) -> str:
    """전용 apply_patch 툴의 패치 본문. 최신 freeform custom_tool_call 은 input 에,
    구버전 function_call 은 arguments(JSON 문자열/딕셔너리)에 담는다."""
    inp = payload.get("input")
    if isinstance(inp, str) and inp:
        return inp
    args = payload.get("arguments")
    if isinstance(args, str):
        try:
            obj = json.loads(args)
        except (json.JSONDecodeError, ValueError):
            return args
        return (obj.get("input") or obj.get("patch") or obj.get("content") or "") \
            if isinstance(obj, dict) else args
    if isinstance(args, dict):
        return args.get("input") or args.get("patch") or args.get("content") or ""
    return ""


def _patch_files_from(text: str) -> list[str]:
    """apply_patch 패치 본문에서 수정/추가/삭제/이동 대상 파일 경로 추출."""
    out: list[str] = []
    for m in _PATCH_FILE_RE.finditer(text or ""):
        p = (m.group(1) or m.group(2) or "").strip()
        if p and p not in out:
            out.append(p)
    return out
