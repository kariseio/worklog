"""compose_full 의 '수집 데이터 원본' 포함/제외 동작."""

from __future__ import annotations

from datetime import date

from worklog import service

_ARGS = dict(
    target=date(2026, 7, 6),
    summary="## 한 줄 요약\n오늘 한 일",
    facts="# 원본 사실 데이터\n- 커밋 목록 등",
    availability="수집 소스: Git ✅",
    analysis_md="## 📊 오늘 지표\n- 커밋 3",
)


def test_excludes_raw_by_default():
    md = service.compose_full(**_ARGS)
    assert "수집 데이터 원본" not in md
    assert "원본 사실 데이터" not in md
    assert "<details>" not in md
    # 수집 소스/저장 대상 표기는 문서에 넣지 않는다
    assert "수집 소스" not in md and "저장 대상" not in md
    # 요약·지표는 그대로 남는다
    assert "한 줄 요약" in md and "오늘 지표" in md


def test_includes_raw_when_flagged():
    md = service.compose_full(**_ARGS, include_raw=True)
    assert "수집 데이터 원본" in md
    assert "원본 사실 데이터" in md
    assert "<details>" in md
