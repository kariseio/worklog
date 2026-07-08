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


def no_window_kwargs() -> dict:
    """콘솔 프로그램(git.EXE, claude.CMD 등)을 subprocess 로 띄울 때 콘솔창이
    깜빡이지 않게 하는 kwargs 를 돌려준다(Windows 전용, 그 외 OS 는 빈 dict).

    CREATE_NO_WINDOW 만으로는 `.cmd`/`.bat` 셔임(예: npm 전역설치된 claude.CMD)이
    콘솔창을 잠깐 띄우는 경우가 있어, STARTUPINFO(SW_HIDE)를 함께 준다.
    """
    if os.name != "nt":
        return {}
    si = subprocess.STARTUPINFO()
    si.dwFlags |= subprocess.STARTF_USESHOWWINDOW
    si.wShowWindow = subprocess.SW_HIDE
    return {"creationflags": CREATE_NO_WINDOW, "startupinfo": si}


def drives_info() -> list[dict]:
    """물리 볼륨만 [{'path': 'C:\\\\', 'label': 'SSD1TB'}, ...] 로 반환.

    Windows: DRIVE_FIXED(3) 중, 심볼릭 링크가 실제 물리 볼륨(\\Device\\HarddiskVolumeN)을
    가리키는 것만. 가상/클라우드 드라이브(예: PantaVDisk, Dokan/WinFsp 계열)는 제외한다.
    실패(예외) 시 존재하는 고정 드라이브 전부(레이블 없이). 그 외 OS: [{'path':'/','label':''}].
    """
    if os.name != "nt":
        return [{"path": "/", "label": ""}]
    import string

    letters = string.ascii_uppercase
    try:
        import ctypes
        from ctypes import create_unicode_buffer

        k32 = ctypes.windll.kernel32
        old_mode = k32.SetErrorMode(0x0001)   # SEM_FAILCRITICALERRORS: 미디어 없음 대화상자 억제
        try:
            out: list[dict] = []
            for c in letters:
                root = f"{c}:\\"
                try:
                    if k32.GetDriveTypeW(root) != 3:          # DRIVE_FIXED 만
                        continue
                    dev = create_unicode_buffer(1024)
                    if not k32.QueryDosDeviceW(f"{c}:", dev, 1024):
                        continue
                    if not dev.value.startswith("\\Device\\HarddiskVolume"):  # 가상/클라우드 제외
                        continue
                    name = create_unicode_buffer(261)
                    ok = k32.GetVolumeInformationW(root, name, 261, None, None, None, None, 0)
                    out.append({"path": root, "label": name.value if ok else ""})
                except Exception:  # noqa: BLE001  개별 드라이브 실패는 건너뜀
                    continue
            if out:
                return out
        finally:
            k32.SetErrorMode(old_mode)
    except Exception:  # noqa: BLE001
        pass
    return [{"path": f"{c}:\\", "label": ""} for c in letters if os.path.exists(f"{c}:\\")]


def fixed_drives() -> list[str]:
    """물리 볼륨 루트 경로 목록. 예: ['C:\\\\', 'D:\\\\']. (drives_info 의 path 만)"""
    return [d["path"] for d in drives_info()]


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
            **no_window_kwargs(),
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
