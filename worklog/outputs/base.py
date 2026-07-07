"""출력(sink) 공통 인터페이스."""

from __future__ import annotations

from dataclasses import dataclass

from ..models import WorkLog


@dataclass
class SinkResult:
    name: str
    ok: bool
    location: str | None = None    # 저장 위치/URL
    error: str | None = None

    @classmethod
    def success(cls, name: str, location: str) -> "SinkResult":
        return cls(name=name, ok=True, location=location)

    @classmethod
    def failure(cls, name: str, error: str) -> "SinkResult":
        return cls(name=name, ok=False, error=error)


class Sink:
    name: str = "sink"

    def write(self, worklog: WorkLog) -> SinkResult:  # pragma: no cover
        raise NotImplementedError
