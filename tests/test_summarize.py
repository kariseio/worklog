"""요약기 provider 해석 테스트."""

from __future__ import annotations

from worklog import summarize
from worklog.config import SummarizerConfig


def test_unknown_provider_warns_and_skips(caplog):
    """오타 등 알 수 없는 provider 는 조용히 넘어가지 않고 경고 후 요약 생략."""
    with caplog.at_level("WARNING"):
        out = summarize.summarize("# facts\n- x", "2026-07-06", SummarizerConfig(provider="claude"))
    assert out is None
    assert any("알 수 없는 summarizer.provider" in r.message for r in caplog.records)


def test_none_provider_skips_silently():
    out = summarize.summarize("# facts", "2026-07-06", SummarizerConfig(provider="none"))
    assert out is None


def test_resolve_known_providers_passthrough():
    assert summarize._resolve_provider("none") == "none"
    assert summarize._resolve_provider("claude_cli") == "claude_cli"
    assert summarize._resolve_provider("anthropic_api") == "anthropic_api"
