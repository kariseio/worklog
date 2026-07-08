"""자동 업데이트 — GitHub Releases 에서 최신 버전을 확인하고 exe 를 통째로 교체한다.

frozen exe(PyInstaller onefile) 로 실행 중일 때만 실제 교체가 동작한다.
소스/pip 실행에서는 확인만 하고 교체는 하지 않는다(exe 가 아니므로).

교체 방식(Windows 는 실행 중 exe 를 못 덮어씀):
  1. 새 exe 를 `<exe>.new` 로 다운로드
  2. 헬퍼 배치가 현재 exe 잠금이 풀릴 때까지 기다렸다가 `.new` → `<exe>` 로 move
  3. 현재 프로세스는 잠시 뒤 종료 → 뮤텍스 해제 → 사용자가 앱을 다시 열면 새 버전이 뜸
(yt-dlp / minio-selfupdate 등이 쓰는 표준 rename-swap 패턴)

재실행(auto-relaunch)은 하지 않는다: frozen exe 를 부모 종료 직후 배치가 다시 띄우면
Job Object/윈도우 스테이션 문제로 실패하거나, 방금 교체된 exe 를 백신이 스캔하는
타이밍과 겹쳐 onefile 추출이 중단돼 'Failed to load Python DLL' 이 뜰 수 있다.
교체만 확실히 하고 UI 에서 '다시 실행'을 안내한다.
"""

from __future__ import annotations

import logging
import os
import shutil
import subprocess
import sys
import tempfile
import threading

from . import __version__

log = logging.getLogger("worklog")

REPO = "kariseio/worklog"
API_LATEST = f"https://api.github.com/repos/{REPO}/releases/latest"

# 배치 실행 플래그: DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP — 부모 종료와 무관하게 배치가 살아남아
# exe 교체(swap)를 끝까지 수행하게 한다.
_DETACHED = 0x00000008 | 0x00000200 if os.name == "nt" else 0

# tasklist/PID 대신 'exe 잠금이 풀릴 때까지 move 재시도' — 콘솔 없이도 안정적.
# 현재 exe 가 실행 중이면 move(=덮어쓰기)가 실패해 .new 가 남고, 프로세스가 죽으면 성공한다.
# 경로는 배치 본문에 넣지 않고 환경변수(WL_NEW/WL_EXE)로 전달 — 한글 경로가 배치 파일
# 인코딩(cmd 는 OEM 코드페이지로 읽음)에 깨지는 것을 방지. 본문은 순수 ASCII 라 안전.
# 교체만 하고 재실행(start)은 하지 않는다 — 재실행 레이스가 'Failed to load Python DLL'
# 를 유발할 수 있어(모듈 docstring 참고), 사용자가 직접 다시 열도록 UI 에서 안내한다.
_UPDATER_BAT = """@echo off
for /l %%i in (1,1,40) do (
  ping -n 2 127.0.0.1 >nul
  move /y "%WL_NEW%" "%WL_EXE%" >nul 2>&1
  if not exist "%WL_NEW%" goto :done
)
:done
del "%~f0"
"""


def is_frozen() -> bool:
    """PyInstaller 등으로 묶인 exe 로 실행 중인지."""
    return bool(getattr(sys, "frozen", False))


def _parse(v: str) -> tuple[int, ...]:
    """'v0.1.3' → (0, 1, 3). 숫자만 관대하게 파싱."""
    out: list[int] = []
    for part in str(v).lstrip("vV").split("."):
        num = ""
        for ch in part:
            if ch.isdigit():
                num += ch
            else:
                break
        out.append(int(num) if num else 0)
    return tuple(out)


def is_newer(latest: str, current: str) -> bool:
    return _parse(latest) > _parse(current)


def _pick_exe_asset(assets: list[dict]) -> str | None:
    """릴리스 에셋 중 exe 다운로드 URL. worklog.exe 우선, 없으면 첫 .exe."""
    if not assets:
        return None
    exact = next((a for a in assets if (a.get("name") or "").lower() == "worklog.exe"), None)
    anyexe = exact or next((a for a in assets if (a.get("name") or "").lower().endswith(".exe")), None)
    return (anyexe or {}).get("browser_download_url")


def check(timeout: float = 6.0) -> dict:
    """최신 릴리스를 확인해 결과 dict 를 반환(네트워크 실패는 error 로만 담고 예외 안 냄)."""
    res = {
        "current": __version__, "latest": None, "update_available": False,
        "download_url": None, "notes": "", "frozen": is_frozen(), "error": None,
    }
    try:
        import requests

        r = requests.get(API_LATEST, timeout=timeout,
                         headers={"Accept": "application/vnd.github+json"})
        if r.status_code != 200:
            res["error"] = f"HTTP {r.status_code}"
            return res
        data = r.json()
        tag = (data.get("tag_name") or "").lstrip("vV")
        res["latest"] = tag or None
        res["notes"] = data.get("body") or ""
        res["download_url"] = _pick_exe_asset(data.get("assets") or [])
        res["update_available"] = bool(tag and is_newer(tag, __version__) and res["download_url"])
    except Exception as e:  # noqa: BLE001
        res["error"] = str(e)
    return res


def download_and_stage(download_url: str, timeout: float = 180.0) -> str:
    """새 exe 를 현재 실행파일 옆 `<exe>.new` 로 내려받고 그 경로를 반환."""
    import requests

    exe = os.path.abspath(sys.executable)
    new = exe + ".new"
    with requests.get(download_url, stream=True, timeout=timeout) as r:
        r.raise_for_status()
        with open(new, "wb") as f:
            for chunk in r.iter_content(65536):
                if chunk:
                    f.write(chunk)
    if os.path.getsize(new) < 1_000_000:   # 정상 exe 는 수십 MB — 너무 작으면 이상
        os.remove(new)
        raise RuntimeError("다운로드 파일이 비정상적으로 작습니다.")
    return new


def cleanup_stale_extractions() -> int:
    r"""이전 실행에서 남은(누출된) onefile 추출 폴더(%TEMP%\_MEIxxxxxx)를 청소한다.

    자동 업데이트가 os._exit 로 즉시 종료하거나 강제 종료되면 PyInstaller 의 임시폴더
    정리(atexit)가 건너뛰어져 폴더가 계속 쌓인다(각 ~50MB). frozen(onefile) 실행일 때만
    동작하며, 다음은 건드리지 않아 안전하다:
      - 현재 실행 중인 폴더(sys._MEIPASS)
      - '사용 중'이라 rmtree 가 실패하는 폴더(다른 실행이 DLL 을 잠금) → skip
      - worklog 서명(worklog/webapp/static)이 없는 폴더(다른 onefile 앱) → skip
    """
    if os.name != "nt" or not is_frozen():
        return 0
    removed = 0
    try:
        mypass = os.path.normcase(os.path.abspath(getattr(sys, "_MEIPASS", "") or ""))
        tmp = tempfile.gettempdir()
        for name in os.listdir(tmp):
            if not name.startswith("_MEI"):
                continue
            d = os.path.join(tmp, name)
            if os.path.normcase(os.path.abspath(d)) == mypass:
                continue
            if not os.path.isdir(os.path.join(d, "worklog", "webapp", "static")):
                continue  # worklog 것이 아니면 skip(타 앱 보호)
            try:
                shutil.rmtree(d)   # 사용 중(잠김)이면 OSError → skip
                removed += 1
            except OSError:
                pass
    except OSError:
        pass
    if removed:
        log.info("이전 업데이트 임시폴더 %d개 정리", removed)
    return removed


def schedule_apply_and_restart(new_exe: str, delay: float = 1.5) -> None:
    """헬퍼 배치(종료 대기 → 교체)를 띄우고, 잠시 뒤 이 프로세스를 종료 예약한다.

    재실행은 하지 않는다(모듈 docstring 참고) — 교체만 확실히 하고 UI 가 '다시 실행'을 안내.
    """
    exe = os.path.abspath(sys.executable)
    bat = os.path.join(tempfile.gettempdir(), "worklog_update.bat")
    with open(bat, "w", encoding="ascii") as f:   # 본문 순수 ASCII (경로는 env 로 전달)
        f.write(_UPDATER_BAT)
    env = dict(os.environ, WL_NEW=os.path.abspath(new_exe), WL_EXE=exe)   # 유니코드 경로 보존
    subprocess.Popen(
        ["cmd", "/c", bat], env=env, creationflags=_DETACHED, close_fds=True,
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    log.info("업데이트 적용: 앱을 닫습니다 (%s → %s)", exe, new_exe)
    threading.Timer(delay, lambda: os._exit(0)).start()
