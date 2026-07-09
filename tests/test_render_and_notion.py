"""render 와 notion 블록 변환 단위 테스트."""

from __future__ import annotations

from datetime import date, datetime, timezone

from worklog.models import (
    CalendarData,
    CalendarEvent,
    ClaudeData,
    ClaudeSession,
    DailyData,
    GitCommit,
    GitData,
)
from worklog.outputs.notion import markdown_to_blocks
from worklog.render import render_facts, render_work_signal
from worklog.util import get_tz


def _sample_data() -> DailyData:
    d = DailyData(target_date=date(2026, 7, 6), tz_name="Asia/Seoul")
    d.calendar = CalendarData(events=[
        CalendarEvent(title="스프린트 회의", start="2026-07-06T10:00:00+09:00",
                      end="2026-07-06T11:00:00+09:00", all_day=False, location="회의실 A",
                      attendees=["김", "이"]),
    ])
    d.git = GitData(commits=[
        GitCommit(repo="repo", hash="abcdef1234", author="me",
                  when=datetime(2026, 7, 6, 1, 0, tzinfo=timezone.utc),
                  subject="fix: login bug", files_changed=2, insertions=10, deletions=3),
    ])
    d.claude = ClaudeData(sessions=[
        ClaudeSession(session_id="s1", project="repo", cwd=r"D:\repo",
                      git_branch="main", title="로그인 버그 수정", intent="로그인 고쳐줘",
                      files_edited=[r"D:\repo\auth.py"], commands=["pytest -q"],
                      tool_counts={"Edit": 1, "Bash": 1}, output_tokens=500),
    ])
    return d


def test_render_facts_has_sections():
    md = render_facts(_sample_data(), get_tz("Asia/Seoul"))
    assert "캘린더 일정" in md
    assert "스프린트 회의" in md
    assert "Git 커밋" in md
    assert "fix: login bug" in md
    assert "Claude Code 작업" in md
    assert "로그인 버그 수정" in md


def test_render_empty():
    d = DailyData(target_date=date(2026, 7, 6), tz_name="Asia/Seoul")
    md = render_facts(d, get_tz("Asia/Seoul"))
    assert "커밋 없음" in md
    assert "세션 없음" in md


def test_markdown_to_blocks():
    md = "# 제목\n\n## 소제목\n\n- 항목1\n- 항목2\n\n일반 문단\n\n```python\nprint(1)\n```\n"
    blocks = markdown_to_blocks(md)
    types = [b["type"] for b in blocks]
    assert "heading_1" in types
    assert "heading_2" in types
    assert types.count("bulleted_list_item") == 2
    assert "paragraph" in types
    assert "code" in types
    code = next(b for b in blocks if b["type"] == "code")
    assert code["code"]["language"] == "python"
    assert code["code"]["rich_text"][0]["text"]["content"] == "print(1)"


def test_work_signal_is_clean():
    """정제 신호: 명령어·원본 프롬프트 제외, 중복 제거, meta 세션(요약기 자신) 제외."""
    d = DailyData(target_date=date(2026, 7, 6), tz_name="Asia/Seoul")
    d.git = GitData(commits=[GitCommit(
        repo="repo", hash="a" * 10, author="me",
        when=datetime(2026, 7, 6, 1, tzinfo=timezone.utc),
        subject="feat: X 추가", files_changed=1, insertions=1, deletions=0)])
    d.claude = ClaudeData(sessions=[
        ClaudeSession(session_id="1", project="repo", cwd=r"D:\repo", git_branch="main",
                      title="로그인 버그 수정", intent="로그인 고쳐줘",
                      files_edited=["a.py", "b.py"], commands=["git commit -m x", "pytest"]),
        ClaudeSession(session_id="2", project="repo", cwd=r"D:\repo", git_branch="main",
                      title="로그인 버그 수정", intent="또", files_edited=["a.py"], commands=["ls"]),
        ClaudeSession(session_id="3", project="Daily Work Log", cwd=r"D:\wl", git_branch="main",
                      title="뭔가", intent="너는 개발자의 하루 활동 로그를 바탕으로 업무일지를 작성",
                      files_edited=[], commands=[]),
    ])
    sig = render_work_signal(d, get_tz("Asia/Seoul"), header="가용 데이터 X")
    assert "feat: X 추가" in sig
    assert sig.count("로그인 버그 수정") == 1          # 중복 제거
    assert "pytest" not in sig and "git commit" not in sig  # 명령어 제외
    assert "로그인 고쳐줘" not in sig                  # 원본 프롬프트 제외
    assert "개발자의 하루 활동 로그" not in sig        # meta 세션 제외
    assert "Daily Work Log" not in sig


def test_meta_session_detection_all_versions():
    """요약기 프롬프트의 어느 버전이든, 그리고 sentinel 이 있으면 meta 로 걸러야 한다."""
    from worklog.render import WORKLOG_SENTINEL, _is_meta_session

    def sess(intent, title=""):
        return ClaudeSession(session_id="s", project="Daily Work Log",
                             cwd=r"D:\study\Daily Work Log", git_branch=None,
                             title=title, intent=intent)

    assert _is_meta_session(sess("너는 개발자의 하루 활동 로그(...)를 바탕으로 ..."))            # v1
    assert _is_meta_session(sess("너는 하루치 개발 활동 데이터를 '업무일지'로 압축하는 도구다."))   # v2
    assert _is_meta_session(sess("너는 하루치 개발 활동 데이터를 '업무일지'로 문서화하는 도구다.")) # v3
    assert _is_meta_session(sess(WORKLOG_SENTINEL + "\n무엇이든"))                              # sentinel
    # 진짜 작업 세션은 meta 가 아니어야 한다
    assert not _is_meta_session(sess("업무일지 생성기를 만들려고 해. activity watch 감시..."))
    assert not _is_meta_session(sess("로그인 버그 고쳐줘", "로그인 버그 수정"))
    # 이 도구를 만드는 세션 — 시그니처 문구가 '중간'에 들어가도 오삭제하면 안 된다(시작 일치만 meta)
    assert not _is_meta_session(sess("render 함수에서 정제된 요약 신호 렌더링 고쳐줘"))
    assert not _is_meta_session(sess("업무일지 본문을 작성하는 로직 수정", "업무일지 렌더 수정"))
    assert not _is_meta_session(sess("요약 프롬프트 고쳐줘 — 하루치 개발 활동 로그 문구 포함"))


def test_rich_text_splits_long_text():
    from worklog.outputs.notion import _rt

    long = "가" * 5000
    rts = _rt(long)
    assert len(rts) >= 3
    assert all(len(r["text"]["content"]) <= 2000 for r in rts)


def test_obsidian_refuses_to_overwrite_foreign_note(tmp_path):
    from worklog.config import ObsidianOutputConfig
    from worklog.models import WorkLog
    from worklog.outputs.obsidian import ObsidianSink

    vault = tmp_path / "vault"
    (vault / "업무일지").mkdir(parents=True)
    existing = vault / "업무일지" / "2026-07-06.md"
    existing.write_text("# 내 데일리노트\n중요한 메모", encoding="utf-8")   # 우리 표식 없는 남의 노트

    wl = WorkLog(target_date=date(2026, 7, 6), facts_markdown="",
                 full_markdown="업무일지 본문", data=None, summary_markdown=None)
    cfg = ObsidianOutputConfig(vault_dir=str(vault), subdir="업무일지")
    res = ObsidianSink(cfg).write(wl)
    assert not res.ok                                            # 남의 노트는 덮어쓰지 않음
    assert "중요한 메모" in existing.read_text(encoding="utf-8")    # 원본 보존

    existing.write_text("---\ndate: 2026-07-06\ntags: [업무일지]\n---\n\n이전본", encoding="utf-8")
    res2 = ObsidianSink(cfg).write(wl)                           # 우리 업무일지면 정상 덮어씀
    assert res2.ok
    assert "업무일지 본문" in existing.read_text(encoding="utf-8")


def test_notion_skips_details_and_tables():
    from worklog.outputs.notion import markdown_to_blocks

    md = "\n".join(["<details>", "<summary>원본</summary>",
                    "| 프로젝트 | 커밋 |", "|---|---|", "| A | 3 |", "</details>"])
    texts = []
    for b in markdown_to_blocks(md):
        rt = b.get(b["type"], {}).get("rich_text", [])
        texts.append("".join(x["text"]["content"] for x in rt))
    joined = " ".join(texts)
    assert "<details>" not in joined and "<summary>" not in joined   # HTML 래퍼 리터럴 안 샘
    assert "---" not in joined                                       # 표 구분선 제거
    assert "A · 3" in joined                                         # 표 행은 가독 변환
