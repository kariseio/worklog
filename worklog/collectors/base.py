"""수집기 공통 인터페이스.

모든 수집기는 `Collector` 를 상속하고 `collect(ctx) -> CollectorResult` 를 구현한다.
수집기는 예외를 던지지 않고 CollectorResult 로 실패/건너뜀을 표현하는 것을 권장한다
(오케스트레이터도 방어적으로 try/except 로 감싼다).
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from datetime import date, datetime


@dataclass
class CollectContext:
    """수집 대상 하루의 경계 정보."""

    target_date: date
    start: datetime          # tz-aware, 그날 00:00
    end: datetime            # tz-aware, 다음날 00:00 (exclusive)
    tz: object               # ZoneInfo | timezone
    tz_name: str
    logger: logging.Logger


@dataclass
class CollectorResult:
    name: str
    data: object | None = None
    warnings: list[str] = field(default_factory=list)
    ok: bool = True
    skipped: bool = False
    skip_reason: str | None = None

    @classmethod
    def skip(cls, name: str, reason: str) -> "CollectorResult":
        return cls(name=name, ok=True, skipped=True, skip_reason=reason)

    @classmethod
    def fail(cls, name: str, reason: str) -> "CollectorResult":
        return cls(name=name, ok=False, warnings=[reason])


class Collector:
    """수집기 베이스."""

    name: str = "collector"

    def collect(self, ctx: CollectContext) -> CollectorResult:  # pragma: no cover
        raise NotImplementedError
