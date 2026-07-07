"""공용 유틸: 시간대/날짜 처리, 기간 계산, 표시 포맷, 로깅, git 저장소 식별."""

from __future__ import annotations

import functools
import logging
import os
import subprocess
from datetime import date, datetime, timedelta

try:
    from zoneinfo import ZoneInfo
except ImportError:  # pragma: no cover  (py<3.9)
    ZoneInfo = None  # type: ignore


log = logging.getLogger("worklog")

# 창(--windowed) 빌드에서 subprocess 가 콘솔창을 띄우지 않도록 하는 플래그(Windows 전용).
CREATE_NO_WINDOW = 0x08000000 if os.name == "nt" else 0


@functools.lru_cache(maxsize=512)
def git_common_dir(path: str) -> str | None:
    """path 가 속한 git 저장소의 공용 .git 경로(realpath)를 반환.

    worktree 든 메인 체크아웃이든 같은 저장소면 동일한 값이 나오므로,
    '물리적으로 같은 저장소'를 식별하는 안정적인 키로 쓴다. git 저장소가 아니면 None.
    """
    if not path:
        return None
    try:
        out = subprocess.run(
            ["git", "-C", path, "rev-parse", "--git-common-dir"],
            capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=5,
            creationflags=CREATE_NO_WINDOW,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if out.returncode != 0:
        return None
    common = out.stdout.strip()
    if not common:
        return None
    if not os.path.isabs(common):
        common = os.path.join(path, common)
    try:
        return os.path.realpath(common)
    except OSError:
        return None


def repo_root_of(common_dir: str) -> str:
    """git-common-dir(…/.git) → 저장소 루트 경로."""
    if os.path.basename(common_dir) == ".git":
        return os.path.dirname(common_dir)
    return common_dir


def get_tz(tz_name: str):
    """시간대 객체를 반환. 실패하면 UTC 로 폴백."""
    if ZoneInfo is None:
        from datetime import timezone

        return timezone.utc
    try:
        return ZoneInfo(tz_name)
    except Exception:
        log.warning("알 수 없는 시간대 %r → UTC 로 대체합니다.", tz_name)
        from datetime import timezone

        return timezone.utc


def resolve_day(date_str: str | None, tz) -> tuple[date, datetime, datetime]:
    """대상 날짜 문자열을 (날짜, 그날 00:00, 다음날 00:00) 로 변환.

    date_str: "YYYY-MM-DD" | "today" | "yesterday" | None(=today)
    반환하는 datetime 은 tz-aware.
    """
    now = datetime.now(tz)
    if date_str in (None, "", "today"):
        target = now.date()
    elif date_str == "yesterday":
        target = (now - timedelta(days=1)).date()
    else:
        target = date.fromisoformat(date_str)

    start = datetime(target.year, target.month, target.day, tzinfo=tz)
    end = start + timedelta(days=1)
    return target, start, end


def parse_iso(ts: str | None):
    """ISO8601(끝의 Z 포함) 문자열을 tz-aware datetime 으로. 실패 시 None."""
    if not ts:
        return None
    try:
        return datetime.fromisoformat(ts.replace("Z", "+00:00"))
    except (ValueError, AttributeError):
        return None


def local_date_of(ts: str | None, tz):
    """UTC/오프셋 타임스탬프 문자열을 로컬 시간대 기준 date 로."""
    dt = parse_iso(ts)
    if dt is None:
        return None
    return dt.astimezone(tz).date()


def human_duration(seconds: float) -> str:
    """초 → '2h 15m' / '45m' / '30s' 형태."""
    seconds = int(round(seconds))
    if seconds < 60:
        return f"{seconds}s"
    minutes, sec = divmod(seconds, 60)
    hours, minutes = divmod(minutes, 60)
    if hours:
        return f"{hours}h {minutes}m"
    return f"{minutes}m"


def fmt_time(dt: datetime | None, tz) -> str:
    """datetime → 'HH:MM' (로컬)."""
    if dt is None:
        return "--:--"
    return dt.astimezone(tz).strftime("%H:%M")


def setup_logging(verbose: bool = False) -> None:
    level = logging.DEBUG if verbose else logging.INFO
    logging.basicConfig(
        level=level,
        format="%(levelname)s %(message)s",
    )
