"""Codex CLI 롤아웃 파서 단위 테스트 (외부 의존 없음).

합성 롤아웃(rollout-*.jsonl)을 만들어 스트리밍 파서를 검증한다.
스키마는 openai/codex GitHub 소스(protocol.rs/models.rs)로 확정한 것.
"""

from __future__ import annotations

import json
import logging
import os
from datetime import date

import pytest

from worklog.collectors.base import CollectContext
from worklog.collectors.codex_logs import CodexCollector
from worklog.config import CodexConfig
from worklog.util import get_tz, resolve_day


def _ctx(target: date, tz_name="Asia/Seoul") -> CollectContext:
    tz = get_tz(tz_name)
    t, start, end = resolve_day(target.isoformat(), tz)
    return CollectContext(
        target_date=t, start=start, end=end, tz=tz, tz_name=tz_name,
        logger=logging.getLogger("test"),
    )


# --- 롤아웃 라인 빌더 (실제 스키마) --- #

def _meta(cwd, branch="main", sid="019c9c21-2a46-77c0-87d8-7cf3716a28e6"):
    return {"timestamp": "2026-07-10T00:12:03.101Z", "type": "session_meta",
            "payload": {"id": sid, "session_id": sid,
                        "timestamp": "2026-07-10T00:12:02.994Z", "cwd": cwd,
                        "originator": "codex_cli_rs", "cli_version": "0.105.0",
                        "source": "cli", "model_provider": "openai",
                        "git": {"commit_hash": "8130207a", "branch": branch,
                                "repository_url": "https://github.com/x/y.git"}}}


def _user(text, ts):
    return {"timestamp": ts, "type": "response_item",
            "payload": {"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": text}]}}


def _assistant(text, ts):
    return {"timestamp": ts, "type": "response_item",
            "payload": {"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": text}]}}


def _call(name, args, ts):
    return {"timestamp": ts, "type": "response_item",
            "payload": {"type": "function_call", "name": name,
                        "arguments": json.dumps(args), "call_id": "call_1"}}


def _custom_call(name, input_text, ts):
    """freeform custom_tool_call — 인자가 payload['input'] 에 평문으로 담긴다(최신 apply_patch)."""
    return {"timestamp": ts, "type": "response_item",
            "payload": {"type": "custom_tool_call", "name": name,
                        "input": input_text, "call_id": "call_2"}}


def _local_shell(command_list, ts):
    """local_shell_call — 커맨드가 payload['action']['command'] 리스트에."""
    return {"timestamp": ts, "type": "response_item",
            "payload": {"type": "local_shell_call",
                        "action": {"type": "exec", "command": command_list}, "call_id": "c3"}}


def _fn_shell(command_list, ts):
    """function_call name='shell' — 커맨드가 arguments(JSON 문자열).command 에."""
    return {"timestamp": ts, "type": "response_item",
            "payload": {"type": "function_call", "name": "shell",
                        "arguments": json.dumps({"command": command_list}), "call_id": "c4"}}


def _tokens(out, ts):
    return {"timestamp": ts, "type": "event_msg",
            "payload": {"type": "token_count",
                        "info": {"total_token_usage": {
                            "input_tokens": 8123, "cached_input_tokens": 4096,
                            "output_tokens": out, "reasoning_output_tokens": 512,
                            "total_tokens": 10015},
                            "last_token_usage": {"output_tokens": 320},
                            "model_context_window": 272000}, "rate_limits": None}}


def _write_rollout(root, name, records, subdir="2026/07/10"):
    d = os.path.join(str(root), *subdir.split("/"))
    os.makedirs(d, exist_ok=True)
    path = os.path.join(d, name)
    with open(path, "w", encoding="utf-8") as f:
        for r in records:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    return path


def _touch(path, ctx):
    os.utime(path, (ctx.start.timestamp() + 3600, ctx.start.timestamp() + 3600))


# --------------------------------------------------------------------------- #

def test_codex_parses_intent_files_tokens(tmp_path):
    root = tmp_path / "sessions"
    recs = [
        _meta(r"D:\demo\repo", "main"),
        # 세션 시작에 주입되는 합성 사용자 메시지 → intent 로 잡히면 안 됨
        _user("<environment_context>\n<cwd>D:\\demo\\repo</cwd>\n</environment_context>",
              "2026-07-10T00:12:03.300Z"),
        _user("로그인 버그 고쳐줘", "2026-07-10T00:12:20.512Z"),
        _assistant("고치겠습니다", "2026-07-10T00:13:00.000Z"),
        _call("shell", {"command": ["pytest", "-q"]}, "2026-07-10T00:13:10.000Z"),
        _call("apply_patch",
              {"input": "*** Begin Patch\n*** Update File: D:\\demo\\repo\\auth.py\n@@\n-x\n+y\n*** End Patch"},
              "2026-07-10T00:13:20.000Z"),
        _tokens(1892, "2026-07-10T00:13:45.010Z"),
    ]
    path = _write_rollout(root, "rollout-2026-07-10T00-12-02-uuid.jsonl", recs)
    ctx = _ctx(date(2026, 7, 10))
    _touch(path, ctx)

    res = CodexCollector(CodexConfig(sessions_dir=str(root))).collect(ctx)
    assert res.ok
    data = res.data
    assert data.total_sessions == 1
    s = data.sessions[0]
    assert s.agent == "codex"
    assert s.intent == "로그인 버그 고쳐줘"          # env-context 는 건너뜀
    assert s.project == "repo"
    assert s.git_branch == "main"
    assert s.session_id == "019c9c21-2a46-77c0-87d8-7cf3716a28e6"
    assert s.output_tokens == 1892                  # 마지막 total_token_usage (합산 아님)
    assert s.tool_counts.get("shell") == 1
    assert s.tool_counts.get("apply_patch") == 1
    assert any("pytest" in c for c in s.commands)
    assert r"D:\demo\repo\auth.py" in s.files_edited
    assert s.qa and s.qa[0].question == "로그인 버그 고쳐줘"
    assert "고치겠습니다" in s.qa[0].answer
    assert data.cwds == [r"D:\demo\repo"]


def test_codex_apply_patch_freeform_custom_tool_call(tmp_path):
    """최신 Codex 의 apply_patch 는 custom_tool_call(payload['input'])로 나온다 — 파일 추출돼야 함."""
    root = tmp_path / "sessions"
    patch = ("*** Begin Patch\n"
             "*** Update File: D:\\demo\\repo\\server.py\n@@\n-a\n+b\n"
             "*** Add File: D:\\demo\\repo\\new.py\n+print(1)\n"
             "*** Delete File: D:\\demo\\repo\\old.py\n"
             "*** End Patch")
    recs = [
        _meta(r"D:\demo\repo"),
        _user("세 파일 고쳐줘", "2026-07-10T03:00:00.000Z"),
        _custom_call("apply_patch", patch, "2026-07-10T03:01:00.000Z"),
        _tokens(100, "2026-07-10T03:02:00.000Z"),
    ]
    path = _write_rollout(root, "rollout-freeform.jsonl", recs)
    ctx = _ctx(date(2026, 7, 10))
    _touch(path, ctx)
    res = CodexCollector(CodexConfig(sessions_dir=str(root))).collect(ctx)
    s = res.data.sessions[0]
    assert s.tool_counts.get("apply_patch") == 1
    assert r"D:\demo\repo\server.py" in s.files_edited
    assert r"D:\demo\repo\new.py" in s.files_edited
    assert r"D:\demo\repo\old.py" in s.files_edited     # Delete File 도 캡처


def test_codex_apply_patch_via_shell(tmp_path):
    """apply_patch 를 셸 툴로 실행한 경우(local_shell_call·function_call name=shell·heredoc)도
    수정 파일이 잡혀야 한다 — 이게 Codex 의 흔한/기본 편집 경로."""
    root = tmp_path / "sessions"
    patch = "*** Begin Patch\n*** Update File: src/app/main.py\n@@\n-a\n+b\n*** End Patch"
    heredoc = "apply_patch <<'EOF'\n" + patch + "\nEOF"
    recs = [
        _meta(r"D:\demo\repo"),
        _user("셸로 패치", "2026-07-10T05:00:00.000Z"),
        _local_shell(["apply_patch", patch], "2026-07-10T05:01:00.000Z"),
        _fn_shell(["apply_patch", patch], "2026-07-10T05:02:00.000Z"),
        _fn_shell(["bash", "-lc", heredoc], "2026-07-10T05:03:00.000Z"),
    ]
    path = _write_rollout(root, "rollout-shellpatch.jsonl", recs)
    ctx = _ctx(date(2026, 7, 10))
    _touch(path, ctx)
    s = CodexCollector(CodexConfig(sessions_dir=str(root))).collect(ctx).data.sessions[0]
    assert "src/app/main.py" in s.files_edited        # 셸 경유로도 추출
    assert s.tool_counts.get("shell") == 3


def test_codex_apply_patch_rename_move_to(tmp_path):
    """rename(Update File + Move to)에서 '이동 대상' 경로가 잡혀야 한다."""
    root = tmp_path / "sessions"
    patch = ("*** Begin Patch\n*** Update File: src/old.py\n*** Move to: src/new.py\n"
             "@@\n-a\n+b\n*** End Patch")
    recs = [_meta(r"D:\demo\repo"),
            _user("리네임", "2026-07-10T06:00:00.000Z"),
            _custom_call("apply_patch", patch, "2026-07-10T06:01:00.000Z")]
    path = _write_rollout(root, "rollout-rename.jsonl", recs)
    ctx = _ctx(date(2026, 7, 10))
    _touch(path, ctx)
    s = CodexCollector(CodexConfig(sessions_dir=str(root))).collect(ctx).data.sessions[0]
    assert "src/new.py" in s.files_edited


def test_codex_missing_token_line(tmp_path):
    """token_count 이벤트가 없으면 output_tokens 는 기본값 0 을 유지(크래시 없음)."""
    root = tmp_path / "sessions"
    recs = [_meta(r"D:\demo\repo"),
            _user("토큰 라인 없음", "2026-07-10T07:00:00.000Z"),
            _assistant("응", "2026-07-10T07:01:00.000Z")]
    path = _write_rollout(root, "rollout-notok.jsonl", recs)
    ctx = _ctx(date(2026, 7, 10))
    _touch(path, ctx)
    s = CodexCollector(CodexConfig(sessions_dir=str(root))).collect(ctx).data.sessions[0]
    assert s.output_tokens == 0


def test_codex_output_tokens_not_summed(tmp_path):
    """token_count 는 누적 스냅샷 — 여러 번 나와도 '마지막' 값을 써야 한다(중복합산 금지)."""
    root = tmp_path / "sessions"
    recs = [
        _meta(r"D:\demo\repo"),
        _user("작업", "2026-07-10T00:10:00.000Z"),
        _assistant("A", "2026-07-10T00:11:00.000Z"),
        _tokens(500, "2026-07-10T00:11:01.000Z"),
        _assistant("B", "2026-07-10T00:12:00.000Z"),
        _tokens(1300, "2026-07-10T00:12:01.000Z"),   # 누적 최신
    ]
    path = _write_rollout(root, "rollout-x.jsonl", recs)
    ctx = _ctx(date(2026, 7, 10))
    _touch(path, ctx)
    res = CodexCollector(CodexConfig(sessions_dir=str(root))).collect(ctx)
    assert res.data.sessions[0].output_tokens == 1300   # 500+1300 이 아님


def test_codex_other_day_ignored(tmp_path):
    root = tmp_path / "sessions"
    recs = [_meta(r"D:\demo\repo"),
            _user("어제 일", "2026-07-09T02:00:00.000Z"),
            _assistant("응", "2026-07-09T02:01:00.000Z")]
    path = _write_rollout(root, "rollout-old.jsonl", recs, subdir="2026/07/09")
    ctx = _ctx(date(2026, 7, 10))
    _touch(path, ctx)   # mtime 통과시켜도 그날 레코드가 없으면 0개
    res = CodexCollector(CodexConfig(sessions_dir=str(root))).collect(ctx)
    assert res.data.total_sessions == 0


def test_codex_midnight_carry(tmp_path):
    """전날 밤 프롬프트의 답이 자정을 넘겨 오늘 시작되면, 그 질문을 이어붙인다."""
    root = tmp_path / "sessions"
    recs = [
        _meta(r"D:\demo\repo"),
        _user("자정 넘기는 질문", "2026-07-09T14:55:00.000Z"),   # KST 07-09 23:55
        _assistant("자정 후 답", "2026-07-09T15:05:00.000Z"),     # KST 07-10 00:05
    ]
    path = _write_rollout(root, "rollout-carry.jsonl", recs)
    ctx = _ctx(date(2026, 7, 10))
    _touch(path, ctx)
    res = CodexCollector(CodexConfig(sessions_dir=str(root))).collect(ctx)
    assert res.data.total_sessions == 1
    s = res.data.sessions[0]
    assert s.intent == "자정 넘기는 질문"
    assert s.qa and "자정 후 답" in s.qa[0].answer


def test_codex_resumed_multiday_only_today(tmp_path):
    """한 롤아웃에 07-09·07-10 레코드가 섞여도(재개 세션), 대상일(07-10) 것만 집계한다."""
    root = tmp_path / "sessions"
    recs = [
        _meta(r"D:\demo\repo"),
        _user("어제 질문", "2026-07-09T05:00:00.000Z"),   # KST 07-09 14:00
        _assistant("어제 답", "2026-07-09T05:01:00.000Z"),
        _user("오늘 질문", "2026-07-10T05:00:00.000Z"),   # KST 07-10 14:00
        _assistant("오늘 답", "2026-07-10T05:01:00.000Z"),
    ]
    path = _write_rollout(root, "rollout-multiday.jsonl", recs)
    ctx = _ctx(date(2026, 7, 10))
    _touch(path, ctx)
    s = CodexCollector(CodexConfig(sessions_dir=str(root))).collect(ctx).data.sessions[0]
    assert s.intent == "오늘 질문"                       # 어제 프롬프트는 intent 아님
    assert len(s.qa) == 1 and s.qa[0].question == "오늘 질문"
    assert "어제" not in (s.qa[0].answer or "")


def test_codex_missing_dir_skips(tmp_path):
    res = CodexCollector(CodexConfig(sessions_dir=str(tmp_path / "nope"))).collect(_ctx(date(2026, 7, 10)))
    assert res.skipped
    assert res.data is None


def test_codex_reads_zst(tmp_path):
    zstd = pytest.importorskip("zstandard")
    root = tmp_path / "sessions"
    d = root / "2026" / "07" / "10"
    d.mkdir(parents=True)
    recs = [_meta(r"D:\demo\repo"),
            _user("압축 세션", "2026-07-10T01:00:00.000Z"),
            _assistant("네", "2026-07-10T01:01:00.000Z"),
            _tokens(50, "2026-07-10T01:02:00.000Z")]
    raw = "\n".join(json.dumps(r, ensure_ascii=False) for r in recs).encode("utf-8")
    path = d / "rollout-2026-07-10T01-00-00-uuid.jsonl.zst"
    path.write_bytes(zstd.ZstdCompressor().compress(raw))
    ctx = _ctx(date(2026, 7, 10))
    os.utime(str(path), (ctx.start.timestamp() + 3600, ctx.start.timestamp() + 3600))
    res = CodexCollector(CodexConfig(sessions_dir=str(root))).collect(ctx)
    assert res.data.total_sessions == 1
    s = res.data.sessions[0]
    assert s.intent == "압축 세션"
    assert s.output_tokens == 50


def test_codex_bad_lines_survive(tmp_path):
    """깨진 JSON 라인이 섞여도 세션 전체가 드롭되지 않는다."""
    root = tmp_path / "sessions"
    d = os.path.join(str(root), "2026", "07", "10")
    os.makedirs(d, exist_ok=True)
    path = os.path.join(d, "rollout-broken.jsonl")
    with open(path, "w", encoding="utf-8") as f:
        f.write(json.dumps(_meta(r"D:\demo\repo"), ensure_ascii=False) + "\n")
        f.write("{ this is not json \n")
        f.write(json.dumps(_user("정상 프롬프트", "2026-07-10T02:00:00.000Z"), ensure_ascii=False) + "\n")
        f.write(json.dumps(_assistant("답", "2026-07-10T02:01:00.000Z"), ensure_ascii=False) + "\n")
    ctx = _ctx(date(2026, 7, 10))
    _touch(path, ctx)
    res = CodexCollector(CodexConfig(sessions_dir=str(root))).collect(ctx)
    assert res.data.total_sessions == 1
    assert res.data.sessions[0].intent == "정상 프롬프트"
